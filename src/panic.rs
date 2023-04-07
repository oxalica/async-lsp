use std::any::Any;
use std::future::Future;
use std::ops::ControlFlow;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tower_layer::Layer;
use tower_service::Service;

use crate::{
    AnyEvent, AnyNotification, AnyRequest, ErrorCode, JsonValue, LspService, ResponseError, Result,
};

pub struct CatchUnwind<S> {
    service: S,
    handler: Handler,
}

type Handler = fn(&str, Box<dyn Any + Send>) -> ResponseError;

impl<S> CatchUnwind<S> {
    pub fn new(service: S) -> Self {
        Self::new_with_handler(service, default_handler)
    }

    pub fn new_with_handler(service: S, handler: Handler) -> Self {
        Self { service, handler }
    }
}

fn default_handler(method: &str, payload: Box<dyn Any + Send>) -> ResponseError {
    let msg = match payload.downcast::<String>() {
        Ok(msg) => *msg,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(msg) => (*msg).into(),
            Err(_payload) => "unknown".into(),
        },
    };
    ResponseError {
        code: ErrorCode::INTERNAL_ERROR,
        message: format!("Request handler of {method} panicked: {msg}"),
        data: None,
    }
}

impl<S: LspService> Service<AnyRequest> for CatchUnwind<S> {
    type Response = JsonValue;
    type Error = ResponseError;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        let method = req.method.clone();
        // FIXME: Clarify conditions of UnwindSafe.
        match catch_unwind(AssertUnwindSafe(|| self.service.call(req)))
            .map_err(|err| (self.handler)(&method, err))
        {
            Ok(fut) => ResponseFuture {
                inner: ResponseFutureInner::Future {
                    fut,
                    method,
                    handler: self.handler,
                },
            },
            Err(err) => ResponseFuture {
                inner: ResponseFutureInner::Ready { err: Some(err) },
            },
        }
    }
}

pin_project! {
    pub struct ResponseFuture<Fut> {
        #[pin]
        inner: ResponseFutureInner<Fut>,
    }
}

pin_project! {
    #[project = ResponseFutureProj]
    enum ResponseFutureInner<Fut> {
        Future {
            #[pin]
            fut: Fut,
            method: String,
            handler: Handler,
        },
        Ready {
            err: Option<ResponseError>,
        },
    }
}

impl<Fut: Future<Output = Result<JsonValue, ResponseError>>> Future for ResponseFuture<Fut> {
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().inner.project() {
            ResponseFutureProj::Future {
                fut,
                method,
                handler,
            } => {
                // FIXME: Clarify conditions of UnwindSafe.
                match catch_unwind(AssertUnwindSafe(|| fut.poll(cx))) {
                    Ok(poll) => poll,
                    Err(payload) => Poll::Ready(Err(handler(method, payload))),
                }
            }
            ResponseFutureProj::Ready { err } => Poll::Ready(Err(err.take().expect("Completed"))),
        }
    }
}

impl<S: LspService> LspService for CatchUnwind<S> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        self.service.notify(notif)
    }

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
        self.service.emit(event)
    }
}

pub struct CatchUnwindLayer {
    handler: Handler,
}

impl Default for CatchUnwindLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl CatchUnwindLayer {
    pub fn new() -> Self {
        Self::new_with_handler(default_handler)
    }

    pub fn new_with_handler(handler: Handler) -> Self {
        Self { handler }
    }
}

impl<S> Layer<S> for CatchUnwindLayer {
    type Service = CatchUnwind<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CatchUnwind::new_with_handler(inner, self.handler)
    }
}
