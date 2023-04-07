use std::future::ready;
use std::ops::ControlFlow;

use futures::future::BoxFuture;
use lsp_types::notification::{self, Notification};
use lsp_types::request::{self, Request};
use lsp_types::{lsp_notification, lsp_request};

use crate::router::Router;
use crate::{Client, Error, ErrorCode, ResponseError, Result};

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

fn notification_not_found<N: Notification>() -> ControlFlow<Result<()>> {
    ControlFlow::Break(Err(Error::Protocol(format!(
        "Unhandled notification: {}",
        N::METHOD,
    ))))
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
        pub trait LanguageServer {
            type Error: From<ResponseError> + Into<ResponseError> + Send + 'static;

            // Requests.

            fn initialize(
                &mut self,
                params: <request::Initialize as Request>::Params,
            ) -> ResponseFuture<request::Initialize, Self::Error>;

            fn shutdown(
                &mut self,
                (): <request::Shutdown as Request>::Params,
            ) -> ResponseFuture<request::Shutdown, Self::Error> {
                Box::pin(ready(Ok(())))
            }

            $(
            fn $req_snake(
                &mut self,
                params: <$req as Request>::Params,
            ) -> ResponseFuture<$req, Self::Error> {
                let _ = params;
                method_not_found::<$req, _>()
            }
            )*

            // Notifications.

            fn exit(
                &mut self,
                (): <notification::Exit as Notification>::Params,
            ) -> ControlFlow<Result<()>> {
                ControlFlow::Break(Ok(()))
            }

            $(
            fn $notif_snake(
                &mut self,
                params: <$notif as Notification>::Params,
            ) -> ControlFlow<Result<()>> {
                let _ = params;
                notification_not_found::<$notif>()
            }
            )*
        }

        impl<S: LanguageServer> Router<S> {
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
        pub trait LanguageClient {
            type Error: Send + 'static;

            // Requests.
            $(
            fn $req_snake(
                &self,
                params: <$req as Request>::Params,
            ) -> BoxFuture<'_, Result<<$req as Request>::Result, Self::Error>>;
            )*

            // Notifications.
            $(
            fn $notif_snake(
                &self,
                params: <$notif as Notification>::Params,
            ) -> BoxFuture<'_, Result<(), Self::Error>>;
            )*
        }

        impl LanguageClient for Client {
            type Error = crate::Error;

            // Requests.
            $(
            fn $req_snake(
                &self,
                params: <$req as Request>::Params,
            ) -> BoxFuture<'_, Result<<$req as Request>::Result, Self::Error>> {
                Box::pin(async move { self.request::<$req>(params).await })
            }
            )*

            // Notifications.
            $(
            fn $notif_snake(
                &self,
                params: <$notif as Notification>::Params,
            ) -> BoxFuture<'_, Result<(), Self::Error>> {
                Box::pin(async move { self.notify::<$notif>(params).await })
            }
            )*
        }
    };
}

include!("./omni_trait_generated.rs");
