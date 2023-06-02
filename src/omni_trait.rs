use std::future::ready;
use std::ops::ControlFlow;

use futures::future::BoxFuture;
use lsp_types::notification::{self, Notification};
use lsp_types::request::{self, Request};
use lsp_types::{lsp_notification, lsp_request};

use crate::router::Router;
use crate::{ClientSocket, ErrorCode, ResponseError, Result, ServerSocket};

use self::sealed::NotifyResult;

mod sealed {
    use super::*;

    pub trait NotifyResult {
        fn fallback<N: Notification>() -> Self;
    }

    impl NotifyResult for ControlFlow<crate::Result<()>> {
        fn fallback<N: Notification>() -> Self {
            if N::METHOD.starts_with("$/")
                || N::METHOD == notification::Exit::METHOD
                || N::METHOD == notification::Initialized::METHOD
            {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(Err(crate::Error::Routing(format!(
                    "Unhandled notification: {}",
                    N::METHOD,
                ))))
            }
        }
    }

    impl NotifyResult for crate::Result<()> {
        fn fallback<N: Notification>() -> Self {
            unreachable!()
        }
    }
}

type ResponseFuture<R, E> = BoxFuture<'static, Result<<R as Request>::Result, E>>;

fn method_not_found<R, E>() -> ResponseFuture<R, E>
where
    R: Request,
    R::Result: Send + 'static,
    E: From<ResponseError> + Send + 'static,
{
    Box::pin(ready(Err(ResponseError {
        code: ErrorCode::METHOD_NOT_FOUND,
        message: format!("No such method: {}", R::METHOD),
        data: None,
    }
    .into())))
}

macro_rules! define {
    (
        { $($req_server:tt, $req_server_snake:ident;)* }
        { $($notif_server:tt, $notif_server_snake:ident;)* }
        { $($req_client:tt, $req_client_snake:ident;)* }
        { $($notif_client:tt, $notif_client_snake:ident;)* }
    ) => {
        define_server! {
            { $($req_server_snake, lsp_request!($req_server);)* }
            { $($notif_server_snake, lsp_notification!($notif_server);)* }
        }
        define_client! {
            { $($req_client_snake, lsp_request!($req_client);)* }
            { $($notif_client_snake, lsp_notification!($notif_client);)* }
        }
    };
}

macro_rules! define_server {
    (
        { $($req_snake:ident, $req:ty;)* }
        { $($notif_snake:ident, $notif:ty;)* }
    ) => {
        /// The omnitrait defining all standard LSP requests and notifications for a Language
        /// Server.
        #[allow(missing_docs)]
        pub trait LanguageServer {
            /// Should always be defined to [`ResponseError`] for user implementations.
            type Error: From<ResponseError> + Send + 'static;
            /// Should always be defined to `ControlFlow<Result<()>>` for user implementations.
            type NotifyResult: NotifyResult;

            // Requests.

            #[must_use]
            fn initialize(
                &mut self,
                params: <request::Initialize as Request>::Params,
            ) -> ResponseFuture<request::Initialize, Self::Error>;

            #[must_use]
            fn shutdown(
                &mut self,
                (): <request::Shutdown as Request>::Params,
            ) -> ResponseFuture<request::Shutdown, Self::Error> {
                Box::pin(ready(Ok(())))
            }

            $(
            #[must_use]
            fn $req_snake(
                &mut self,
                params: <$req as Request>::Params,
            ) -> ResponseFuture<$req, Self::Error> {
                let _ = params;
                method_not_found::<$req, _>()
            }
            )*

            // Notifications.

            #[must_use]
            fn initialized(
                &mut self,
                params: <notification::Initialized as Notification>::Params,
            ) -> Self::NotifyResult {
                let _ = params;
                Self::NotifyResult::fallback::<notification::Initialized>()
            }

            #[must_use]
            fn exit(
                &mut self,
                (): <notification::Exit as Notification>::Params,
            ) -> Self::NotifyResult {
                Self::NotifyResult::fallback::<notification::Exit>()
            }

            $(
            #[must_use]
            fn $notif_snake(
                &mut self,
                params: <$notif as Notification>::Params,
            ) -> Self::NotifyResult {
                let _ = params;
                Self::NotifyResult::fallback::<$notif>()
            }
            )*
        }

        macro_rules! impl_server_socket {
            ($ty:ty) => {
                impl LanguageServer for $ty {
                    type Error = crate::Error;
                    type NotifyResult = crate::Result<()>;

                    // Requests.

                    fn initialize(
                        &mut self,
                        params: <request::Initialize as Request>::Params,
                    ) -> ResponseFuture<request::Initialize, Self::Error> {
                        Box::pin(self.0.request::<request::Initialize>(params))
                    }

                    fn shutdown(
                        &mut self,
                        (): <request::Shutdown as Request>::Params,
                    ) -> ResponseFuture<request::Shutdown, Self::Error> {
                        Box::pin(self.0.request::<request::Shutdown>(()))
                    }

                    $(
                    fn $req_snake(
                        &mut self,
                        params: <$req as Request>::Params,
                    ) -> ResponseFuture<$req, Self::Error> {
                        Box::pin(self.0.request::<$req>(params))
                    }
                    )*

                    // Notifications.

                    fn initialized(
                        &mut self,
                        params: <notification::Initialized as Notification>::Params,
                    ) -> Self::NotifyResult {
                        self.notify::<notification::Initialized>(params)
                    }

                    fn exit(
                        &mut self,
                        (): <notification::Exit as Notification>::Params,
                    ) -> Self::NotifyResult {
                        self.notify::<notification::Exit>(())
                    }

                    $(
                    fn $notif_snake(
                        &mut self,
                        params: <$notif as Notification>::Params,
                    ) -> Self::NotifyResult {
                        self.notify::<$notif>(params)
                    }
                    )*
                }
            };
        }

        impl_server_socket!(ServerSocket);
        impl_server_socket!(&'_ ServerSocket);

        impl<S> Router<S>
        where
            S: LanguageServer<NotifyResult = ControlFlow<crate::Result<()>>>,
            ResponseError: From<S::Error>,
        {
            /// Create a [`Router`] using its implementation of [`LanguageServer`] as handlers.
            #[must_use]
            pub fn from_language_server(state: S) -> Self {
                let mut this = Self::new(state);
                this.request::<request::Initialize, _>(|state, params| {
                    let fut = state.initialize(params);
                    async move { fut.await.map_err(Into::into) }
                });
                this.request::<request::Shutdown, _>(|state, params| {
                    let fut = state.shutdown(params);
                    async move { fut.await.map_err(Into::into) }
                });
                $(this.request::<$req, _>(|state, params| {
                    let fut = state.$req_snake(params);
                    async move { fut.await.map_err(Into::into) }
                });)*
                this.notification::<notification::Initialized>(|state, params| state.initialized(params));
                this.notification::<notification::Exit>(|state, params| state.exit(params));
                $(this.notification::<$notif>(|state, params| state.$notif_snake(params));)*
                this
            }
        }
    };
}

macro_rules! define_client {
    (
        { $($req_snake:ident, $req:ty;)* }
        { $($notif_snake:ident, $notif:ty;)* }
    ) => {
        /// The omnitrait defining all standard LSP requests and notifications for a Language
        /// Client.
        #[allow(missing_docs)]
        pub trait LanguageClient {
            /// Should always be defined to [`ResponseError`] for user implementations.
            type Error: From<ResponseError> + Send + 'static;
            /// Should always be defined to `ControlFlow<Result<()>>` for user implementations.
            type NotifyResult: NotifyResult;

            // Requests.
            $(
            #[must_use]
            fn $req_snake(
                &mut self,
                params: <$req as Request>::Params,
            ) -> ResponseFuture<$req, Self::Error> {
                let _ = params;
                method_not_found::<$req, _>()
            }
            )*

            // Notifications.
            $(
            #[must_use]
            fn $notif_snake(
                &mut self,
                params: <$notif as Notification>::Params,
            ) -> Self::NotifyResult {
                let _ = params;
                Self::NotifyResult::fallback::<$notif>()
            }
            )*
        }

        macro_rules! impl_client_socket {
            ($ty:ty) => {
                impl LanguageClient for $ty {
                    type Error = crate::Error;
                    type NotifyResult = crate::Result<()>;

                    // Requests.
                    $(
                    fn $req_snake(
                        &mut self,
                        params: <$req as Request>::Params,
                    ) -> ResponseFuture<$req, Self::Error> {
                        Box::pin(self.0.request::<$req>(params))
                    }
                    )*

                    // Notifications.
                    $(
                    fn $notif_snake(
                        &mut self,
                        params: <$notif as Notification>::Params,
                    ) -> Self::NotifyResult {
                        self.notify::<$notif>(params)
                    }
                    )*
                }
            };
        }

        impl_client_socket!(ClientSocket);
        impl_client_socket!(&'_ ClientSocket);

        impl<S> Router<S>
        where
            S: LanguageClient<NotifyResult = ControlFlow<crate::Result<()>>>,
            ResponseError: From<S::Error>,
        {
            /// Create a [`Router`] using its implementation of [`LanguageClient`] as handlers.
            #[must_use]
            pub fn from_language_client(state: S) -> Self {
                let mut this = Self::new(state);
                $(this.request::<$req, _>(|state, params| {
                    let fut = state.$req_snake(params);
                    async move { fut.await.map_err(Into::into) }
                });)*
                $(this.notification::<$notif>(|state, params| state.$notif_snake(params));)*
                this
            }
        }
    };
}

include!("./omni_trait_generated.rs");
