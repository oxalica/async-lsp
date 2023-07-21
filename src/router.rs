//! Dispatch requests and notifications to individual handlers.
use std::any::TypeId;
use std::collections::HashMap;
use std::future::{ready, Future};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{Context, Poll};

use lsp_types::notification::Notification;
use lsp_types::request::Request;
use tower_service::Service;

use crate::{
    AnyEvent, AnyNotification, AnyRequest, ErrorCode, JsonValue, LspService, ResponseError, Result,
};

/// A router dispatching requests and notifications to individual handlers.
pub struct Router<St, Error = ResponseError> {
    state: St,
    req_handlers: HashMap<&'static str, BoxReqHandler<St, Error>>,
    notif_handlers: HashMap<&'static str, BoxNotifHandler<St>>,
    event_handlers: HashMap<TypeId, BoxEventHandler<St>>,
    unhandled_req: BoxReqHandler<St, Error>,
    unhandled_notif: BoxNotifHandler<St>,
    unhandled_event: BoxEventHandler<St>,
}

type BoxReqFuture<Error> = Pin<Box<dyn Future<Output = Result<JsonValue, Error>> + Send>>;
type BoxReqHandler<St, Error> = Box<dyn Fn(&mut St, AnyRequest) -> BoxReqFuture<Error> + Send>;
type BoxNotifHandler<St> = Box<dyn Fn(&mut St, AnyNotification) -> ControlFlow<Result<()>> + Send>;
type BoxEventHandler<St> = Box<dyn Fn(&mut St, AnyEvent) -> ControlFlow<Result<()>> + Send>;

impl<St, Error> Default for Router<St, Error>
where
    St: Default,
    Error: From<ResponseError> + Send + 'static,
{
    fn default() -> Self {
        Self::new(St::default())
    }
}

// TODO: Make it possible to construct with arbitrary `Error`, with no default handlers.
impl<St, Error> Router<St, Error>
where
    Error: From<ResponseError> + Send + 'static,
{
    /// Create a empty `Router`.
    #[must_use]
    pub fn new(state: St) -> Self {
        Self {
            state,
            req_handlers: HashMap::new(),
            notif_handlers: HashMap::new(),
            event_handlers: HashMap::new(),
            unhandled_req: Box::new(|_, req| {
                Box::pin(ready(Err(ResponseError {
                    code: ErrorCode::METHOD_NOT_FOUND,
                    message: format!("No such method {}", req.method()),
                    data: None,
                }
                .into())))
            }),
            unhandled_notif: Box::new(|_, notif| {
                if notif.method().starts_with("$/") {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(Err(crate::Error::Routing(format!(
                        "Unhandled notification: {}",
                        notif.method(),
                    ))))
                }
            }),
            unhandled_event: Box::new(|_, event| {
                ControlFlow::Break(Err(crate::Error::Routing(format!(
                    "Unhandled event: {event:?}"
                ))))
            }),
        }
    }

    /// Add an asynchronous request handler for a specific LSP request `R`.
    ///
    /// If handler for the method already exists, it replaces the old one.
    pub fn request<R: Request, Fut>(
        &mut self,
        handler: impl Fn(&mut St, R::Params) -> Fut + Send + 'static,
    ) -> &mut Self
    where
        Fut: Future<Output = Result<R::Result, Error>> + Send + 'static,
    {
        self.req_handlers.insert(
            R::METHOD,
            Box::new(move |state, req| {
                match serde_json::from_str::<R::Params>(req.params().get()) {
                    Ok(params) => {
                        let fut = handler(state, params);
                        Box::pin(async move {
                            Ok(serde_json::to_value(fut.await?).expect("Serialization failed"))
                        })
                    }
                    Err(err) => Box::pin(ready(Err(ResponseError {
                        code: ErrorCode::INVALID_PARAMS,
                        message: format!("Failed to deserialize parameters: {err}"),
                        data: None,
                    }
                    .into()))),
                }
            }),
        );
        self
    }

    /// Add a synchronous request handler for a specific LSP notification `N`.
    ///
    /// If handler for the method already exists, it replaces the old one.
    pub fn notification<N: Notification>(
        &mut self,
        handler: impl Fn(&mut St, N::Params) -> ControlFlow<Result<()>> + Send + 'static,
    ) -> &mut Self {
        self.notif_handlers.insert(
            N::METHOD,
            Box::new(move |state, notif| {
                match serde_json::from_str::<N::Params>(notif.params().get()) {
                    Ok(params) => handler(state, params),
                    Err(err) => ControlFlow::Break(Err(err.into())),
                }
            }),
        );
        self
    }

    /// Add a synchronous event handler for event type `E`.
    ///
    /// If handler for the method already exists, it replaces the old one.
    pub fn event<E: Send + 'static>(
        &mut self,
        handler: impl Fn(&mut St, E) -> ControlFlow<Result<()>> + Send + 'static,
    ) -> &mut Self {
        self.event_handlers.insert(
            TypeId::of::<E>(),
            Box::new(move |state, event| {
                let event = event.downcast::<E>().expect("Checked TypeId");
                handler(state, event)
            }),
        );
        self
    }

    /// Set an asynchronous catch-all request handler for any requests with no corresponding handler
    /// for its `method`.
    ///
    /// There can only be a single catch-all request handler. New ones replace old ones.
    ///
    /// The default handler is to respond a error response with code
    /// [`ErrorCode::METHOD_NOT_FOUND`].
    pub fn unhandled_request<Fut>(
        &mut self,
        handler: impl Fn(&mut St, AnyRequest) -> Fut + Send + 'static,
    ) -> &mut Self
    where
        Fut: Future<Output = Result<JsonValue, Error>> + Send + 'static,
    {
        self.unhandled_req = Box::new(move |state, req| Box::pin(handler(state, req)));
        self
    }

    /// Set a synchronous catch-all notification handler for any notifications with no
    /// corresponding handler for its `method`.
    ///
    /// There can only be a single catch-all notification handler. New ones replace old ones.
    ///
    /// The default handler is to do nothing for methods starting with `$/`, and break the main
    /// loop with [`Error::Routing`][crate::Error::Routing] for other methods. Typically
    /// notifications are critical and
    /// losing them can break state synchronization, easily leading to catastrophic failures after
    /// incorrect incremental changes.
    pub fn unhandled_notification(
        &mut self,
        handler: impl Fn(&mut St, AnyNotification) -> ControlFlow<Result<()>> + Send + 'static,
    ) -> &mut Self {
        self.unhandled_notif = Box::new(handler);
        self
    }

    /// Set a synchronous catch-all event handler for any notifications with no
    /// corresponding handler for its type.
    ///
    /// There can only be a single catch-all event handler. New ones replace old ones.
    ///
    /// The default handler is to break the main loop with
    /// [`Error::Routing`][crate::Error::Routing]. Since events are
    /// emitted internally, mishandling are typically logic errors.
    pub fn unhandled_event(
        &mut self,
        handler: impl Fn(&mut St, AnyEvent) -> ControlFlow<Result<()>> + Send + 'static,
    ) -> &mut Self {
        self.unhandled_event = Box::new(handler);
        self
    }
}

impl<St, Error> Service<AnyRequest> for Router<St, Error> {
    type Response = JsonValue;
    type Error = Error;
    type Future = BoxReqFuture<Error>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        let h = self
            .req_handlers
            .get(req.method())
            .unwrap_or(&self.unhandled_req);
        h(&mut self.state, req)
    }
}

impl<St> LspService for Router<St> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        let h = self
            .notif_handlers
            .get(notif.method())
            .unwrap_or(&self.unhandled_notif);
        h(&mut self.state, notif)
    }

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
        let h = self
            .event_handlers
            .get(&event.inner_type_id())
            .unwrap_or(&self.unhandled_event);
        h(&mut self.state, event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn _assert_send<St: Send>(router: Router<St>) -> impl Send {
        router
    }
}
