use std::collections::HashMap;
use std::future::{ready, Future};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{Context, Poll};

use lsp_server::ErrorCode;
use lsp_types::notification::Notification;
use lsp_types::request::Request;
use tower_service::Service;

use crate::{AnyNotification, AnyRequest, Error, JsonValue, LspService, ResponseError, Result};

pub struct Router<S> {
    state: S,
    req_handlers: HashMap<&'static str, BoxReqHandler<S>>,
    notif_handlers: HashMap<&'static str, BoxNotifHandler<S>>,
    unhandled_req: BoxReqHandler<S>,
    unhandled_notif: BoxNotifHandler<S>,
}

type BoxReqFuture = Pin<Box<dyn Future<Output = Result<JsonValue, ResponseError>> + Send>>;
type BoxReqHandler<S> = Box<dyn Fn(&mut S, AnyRequest) -> BoxReqFuture>;
type BoxNotifHandler<S> = Box<dyn Fn(&mut S, AnyNotification) -> ControlFlow<Result<()>>>;

impl<S: Default> Default for Router<S> {
    fn default() -> Self {
        Self::new(S::default())
    }
}

impl<S> Router<S> {
    pub fn new(state: S) -> Self {
        Self {
            state,
            req_handlers: HashMap::new(),
            notif_handlers: HashMap::new(),
            unhandled_req: Box::new(|_, req| {
                Box::pin(ready(Err(ResponseError {
                    code: ErrorCode::MethodNotFound as _,
                    message: format!("No such method {}", req.method),
                    data: None,
                })))
            }),
            unhandled_notif: Box::new(|_, notif| {
                if notif.method.starts_with("$/") {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(Err(Error::Protocol(format!(
                        "Unhandled notification: {}",
                        notif.method,
                    ))))
                }
            }),
        }
    }

    pub fn request<R: Request, Fut>(
        &mut self,
        handler: impl Fn(&mut S, R::Params) -> Fut + 'static,
    ) -> &mut Self
    where
        Fut: Future<Output = Result<R::Result, ResponseError>> + Send + 'static,
    {
        self.unhandled_req = Box::new(|_, _| todo!());
        self.req_handlers.insert(
            R::METHOD,
            Box::new(
                move |service, req| match serde_json::from_value::<R::Params>(req.params) {
                    Ok(params) => {
                        let fut = handler(service, params);
                        Box::pin(async move {
                            Ok(serde_json::to_value(fut.await?).expect("Serialization failed"))
                        })
                    }
                    Err(err) => Box::pin(ready(Err(ResponseError {
                        code: ErrorCode::InvalidParams as _,
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
        handler: impl Fn(&mut S, N::Params) -> ControlFlow<Result<()>> + 'static,
    ) -> &mut Self {
        self.notif_handlers.insert(
            N::METHOD,
            Box::new(move |service, notif| {
                match serde_json::from_value::<N::Params>(notif.params) {
                    Ok(params) => handler(service, params),
                    Err(err) => ControlFlow::Break(Err(err.into())),
                }
            }),
        );
        self
    }

    pub fn unhandled_request<Fut>(
        &mut self,
        handler: impl Fn(&mut S, AnyRequest) -> Fut + 'static,
    ) -> &mut Self
    where
        Fut: Future<Output = Result<JsonValue, ResponseError>> + Send + 'static,
    {
        self.unhandled_req = Box::new(move |service, req| Box::pin(handler(service, req)));
        self
    }

    pub fn unhandled_notification(
        &mut self,
        handler: impl Fn(&mut S, AnyNotification) -> ControlFlow<Result<()>> + 'static,
    ) -> &mut Self {
        self.unhandled_notif = Box::new(handler);
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
}
