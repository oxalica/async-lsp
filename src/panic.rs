//! Catch panics of underlying handlers and turn them into error responses.
//!
//! *Applies to both Language Servers and Language Clients.*
use std::any::Any;
use std::future::Future;
use std::ops::ControlFlow;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tower_layer::Layer;
use tower_service::Service;

use crate::{AnyEvent, AnyNotification, AnyRequest, ErrorCode, LspService, ResponseError, Result};

/// The middleware catching panics of underlying handlers and turn them into error responses.
///
/// See [module level documentations](self) for details.
pub struct CatchUnwind<S: LspService> {
    service: S,
    handler: Handler<S::Error>,
}

define_getters!(impl[S: LspService] CatchUnwind<S>, service: S);

type Handler<E> = fn(method: &str, payload: Box<dyn Any + Send>) -> E;

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
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        let method = req.method().to_owned();
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
    /// The [`Future`] type used by the [`CatchUnwind`] middleware.
    pub struct ResponseFuture<Fut, Error> {
        #[pin]
        inner: ResponseFutureInner<Fut, Error>,
    }
}

pin_project! {
    #[project = ResponseFutureProj]
    enum ResponseFutureInner<Fut, Error> {
        Future {
            #[pin]
            fut: Fut,
            method: String,
            handler: Handler<Error>,
        },
        Ready {
            err: Option<Error>,
        },
    }
}

impl<Response, Fut, Error> Future for ResponseFuture<Fut, Error>
where
    Fut: Future<Output = Result<Response, Error>>,
{
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

/// The builder of [`CatchUnwind`] middleware.
///
/// It's [`Default`] configuration tries to downcast the panic payload into `String` or `&str`, and
/// fallback to format it via [`std::fmt::Display`], as the error message.
/// The error code is set to [`ErrorCode::INTERNAL_ERROR`].
#[derive(Clone)]
#[must_use]
pub struct CatchUnwindBuilder<Error = ResponseError> {
    handler: Handler<Error>,
}

impl Default for CatchUnwindBuilder<ResponseError> {
    fn default() -> Self {
        Self::new_with_handler(default_handler)
    }
}

impl<Error> CatchUnwindBuilder<Error> {
    /// Create the builder of [`CatchUnwind`] middleware with a custom handler converting panic
    /// payloads into [`ResponseError`].
    pub fn new_with_handler(handler: Handler<Error>) -> Self {
        Self { handler }
    }
}

/// A type alias of [`CatchUnwindBuilder`] conforming to the naming convention of [`tower_layer`].
pub type CatchUnwindLayer<Error = ResponseError> = CatchUnwindBuilder<Error>;

impl<S: LspService> Layer<S> for CatchUnwindBuilder<S::Error> {
    type Service = CatchUnwind<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CatchUnwind {
            service: inner,
            handler: self.handler,
        }
    }
}
