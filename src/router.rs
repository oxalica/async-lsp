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
    AnyEvent, AnyNotification, AnyRequest, Error, ErrorCode, JsonValue, LspService, ResponseError,
    Result,
};

pub struct Router<S> {
    state: S,
    req_handlers: HashMap<&'static str, BoxReqHandler<S>>,
    notif_handlers: HashMap<&'static str, BoxNotifHandler<S>>,
    event_handlers: HashMap<TypeId, BoxEventHandler<S>>,
    unhandled_req: BoxReqHandler<S>,
    unhandled_notif: BoxNotifHandler<S>,
    unhandled_event: BoxEventHandler<S>,
}

type BoxReqFuture = Pin<Box<dyn Future<Output = Result<JsonValue, ResponseError>> + Send>>;
type BoxReqHandler<S> = Box<dyn Fn(&mut S, AnyRequest) -> BoxReqFuture + Send>;
type BoxNotifHandler<S> = Box<dyn Fn(&mut S, AnyNotification) -> ControlFlow<Result<()>> + Send>;
type BoxEventHandler<S> = Box<dyn Fn(&mut S, AnyEvent) -> ControlFlow<Result<()>> + Send>;

impl<S: Default> Default for Router<S> {
    fn default() -> Self {
        Self::new(S::default())
    }
}

impl<S> Router<S> {
    #[must_use]
    pub fn new(state: S) -> Self {
        Self {
            state,
            req_handlers: HashMap::new(),
            notif_handlers: HashMap::new(),
            event_handlers: HashMap::new(),
            unhandled_req: Box::new(|_, req| {
                Box::pin(ready(Err(ResponseError {
                    code: ErrorCode::METHOD_NOT_FOUND,
                    message: format!("No such method {}", req.method),
                    data: None,
                })))
            }),
            unhandled_notif: Box::new(|_, notif| {
                if notif.method.starts_with("$/") {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(Err(Error::Routing(format!(
                        "Unhandled notification: {}",
                        notif.method,
                    ))))
                }
            }),
            unhandled_event: Box::new(|_, event| {
                ControlFlow::Break(Err(Error::Routing(format!("Unhandled event: {event:?}"))))
            }),
        }
    }

    pub fn request<R: Request, Fut>(
        &mut self,
        handler: impl Fn(&mut S, R::Params) -> Fut + Send + 'static,
    ) -> &mut Self
    where
        Fut: Future<Output = Result<R::Result, ResponseError>> + Send + 'static,
    {
        self.req_handlers.insert(
            R::METHOD,
            Box::new(
                move |state, req| match serde_json::from_value::<R::Params>(req.params) {
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
                    }))),
                },
            ),
        );
        self
    }

    pub fn notification<N: Notification>(
        &mut self,
        handler: impl Fn(&mut S, N::Params) -> ControlFlow<Result<()>> + Send + 'static,
    ) -> &mut Self {
        self.notif_handlers.insert(
            N::METHOD,
            Box::new(
                move |state, notif| match serde_json::from_value::<N::Params>(notif.params) {
                    Ok(params) => handler(state, params),
                    Err(err) => ControlFlow::Break(Err(err.into())),
                },
            ),
        );
        self
    }

    pub fn event<E: Send + 'static>(
        &mut self,
        handler: impl Fn(&mut S, E) -> ControlFlow<Result<()>> + Send + 'static,
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

    pub fn unhandled_request<Fut>(
        &mut self,
        handler: impl Fn(&mut S, AnyRequest) -> Fut + Send + 'static,
    ) -> &mut Self
    where
        Fut: Future<Output = Result<JsonValue, ResponseError>> + Send + 'static,
    {
        self.unhandled_req = Box::new(move |state, req| Box::pin(handler(state, req)));
        self
    }

    pub fn unhandled_notification(
        &mut self,
        handler: impl Fn(&mut S, AnyNotification) -> ControlFlow<Result<()>> + Send + 'static,
    ) -> &mut Self {
        self.unhandled_notif = Box::new(handler);
        self
    }

    pub fn unhandled_event(
        &mut self,
        handler: impl Fn(&mut S, AnyEvent) -> ControlFlow<Result<()>> + Send + 'static,
    ) -> &mut Self {
        self.unhandled_event = Box::new(handler);
        self
    }
}

impl<S> Service<AnyRequest> for Router<S> {
    type Response = JsonValue;
    type Error = ResponseError;
    type Future = BoxReqFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        let h = self
            .req_handlers
            .get(&*req.method)
            .unwrap_or(&self.unhandled_req);
        h(&mut self.state, req)
    }
}

impl<S> LspService for Router<S> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        let h = self
            .notif_handlers
            .get(&*notif.method)
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

    fn _assert_send<S: Send>(router: Router<S>) -> impl Send {
        router
    }
}
