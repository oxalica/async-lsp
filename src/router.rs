use std::collections::HashMap;
use std::future::{ready, Future};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{Context, Poll};

use lsp_server::ErrorCode;
use lsp_types::notification::Notification;
use lsp_types::request::Request;
use tower::Service;

use crate::{AnyNotification, AnyRequest, Error, JsonValue, LspService, ResponseError, Result};

#[derive(Default)]
pub struct Router<S> {
    state: S,
    req_handlers: HashMap<&'static str, BoxReqHandler<S>>,
    notif_handlers: HashMap<&'static str, BoxNotifHandler<S>>,
}

type BoxReqFuture = Pin<Box<dyn Future<Output = Result<JsonValue, ResponseError>> + Send>>;
type BoxReqHandler<S> = Box<dyn Fn(&mut S, AnyRequest) -> BoxReqFuture>;
type BoxNotifHandler<S> = Box<dyn Fn(&mut S, AnyNotification) -> ControlFlow<Result<()>>>;

impl<S> Router<S> {
    pub fn new(state: S) -> Self {
        Self {
            state,
            req_handlers: HashMap::new(),
            notif_handlers: HashMap::new(),
        }
    }

    pub fn request<R: Request, Fut>(
        &mut self,
        handler: impl Fn(&mut S, R::Params) -> Fut + 'static,
    ) -> &mut Self
    where
        Fut: Future<Output = Result<R::Result, ResponseError>> + Send + 'static,
    {
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
}

impl<S> Service<AnyRequest> for Router<S> {
    type Response = JsonValue;
    type Error = ResponseError;
    type Future = BoxReqFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        match self.req_handlers.get(&*req.method) {
            Some(h) => h(&mut self.state, req),
            None => Box::pin(ready(Err(ResponseError {
                code: ErrorCode::MethodNotFound as _,
                message: format!("No such method {}", req.method),
                data: None,
            }))),
        }
    }
}

impl<S> LspService for Router<S> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        match self.notif_handlers.get(&*notif.method) {
            Some(h) => h(&mut self.state, notif),
            None => ControlFlow::Break(Err(Error::Protocol(format!(
                "Unhandled notification {}",
                notif.method
            )))),
        }
    }
}
