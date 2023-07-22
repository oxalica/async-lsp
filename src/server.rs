//! Language Server lifecycle.
//!
//! *Only applies to Language Servers.*
//!
//! This middleware handles
//! [the lifecycle of Language Servers](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#lifeCycleMessages),
//! specifically:
//! - Exit the main loop with `ControlFlow::Break(Ok(()))` on `exit` notification.
//! - Responds unrelated requests with errors and ignore unrelated notifications during
//!   initialization and shutting down.
use std::future::{ready, Future, Ready};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::Either;
use lsp_types::notification::{self, Notification};
use lsp_types::request::{self, Request};
use pin_project_lite::pin_project;
use tower_layer::Layer;
use tower_service::Service;

use crate::{
    AnyEvent, AnyNotification, AnyRequest, Error, ErrorCode, LspService, ResponseError, Result,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Uninitialized,
    Initializing,
    Ready,
    ShuttingDown,
}

/// The middleware handling Language Server lifecycle.
///
/// See [module level documentations](self) for details.
#[derive(Debug, Default)]
pub struct Lifecycle<S> {
    service: S,
    state: State,
}

define_getters!(impl[S] Lifecycle<S>, service: S);

impl<S> Lifecycle<S> {
    /// Creating the `Lifecycle` middleware in uninitialized state.
    #[must_use]
    pub fn new(service: S) -> Self {
        Self {
            service,
            state: State::Uninitialized,
        }
    }
}

impl<S: LspService> Service<AnyRequest> for Lifecycle<S> {
    type Response = S::Response;
    type Error = ResponseError;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        let inner = match (self.state, &*req.method) {
            (State::Uninitialized, request::Initialize::METHOD) => {
                self.state = State::Initializing;
                Either::Left(self.service.call(req))
            }
            (State::Uninitialized | State::Initializing, _) => {
                Either::Right(ready(Err(ResponseError {
                    code: ErrorCode::SERVER_NOT_INITIALIZED,
                    message: "Server is not initialized yet".into(),
                    data: None,
                })))
            }
            (_, request::Initialize::METHOD) => Either::Right(ready(Err(ResponseError {
                code: ErrorCode::INVALID_REQUEST,
                message: "Server is already initialized".into(),
                data: None,
            }))),
            (State::Ready, _) => {
                if req.method == request::Shutdown::METHOD {
                    self.state = State::ShuttingDown;
                }
                Either::Left(self.service.call(req))
            }
            (State::ShuttingDown, _) => Either::Right(ready(Err(ResponseError {
                code: ErrorCode::INVALID_REQUEST,
                message: "Server is shutting down".into(),
                data: None,
            }))),
        };
        ResponseFuture { inner }
    }
}

impl<S: LspService> LspService for Lifecycle<S> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        match &*notif.method {
            notification::Exit::METHOD => {
                self.service.notify(notif)?;
                ControlFlow::Break(Ok(()))
            }
            notification::Initialized::METHOD => {
                if self.state != State::Initializing {
                    return ControlFlow::Break(Err(Error::Protocol(format!(
                        "Unexpected initialized notification on state {:?}",
                        self.state
                    ))));
                }
                self.state = State::Ready;
                self.service.notify(notif)?;
                ControlFlow::Continue(())
            }
            _ => self.service.notify(notif),
        }
    }

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
        self.service.emit(event)
    }
}

pin_project! {
    /// The [`Future`] type used by the [`Lifecycle`] middleware.
    pub struct ResponseFuture<Fut: Future> {
        #[pin]
        inner: Either<Fut, Ready<Fut::Output>>,
    }
}

impl<Fut: Future> Future for ResponseFuture<Fut> {
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(cx)
    }
}

/// A [`tower_layer::Layer`] which builds [`Lifecycle`].
#[must_use]
#[derive(Clone, Default)]
pub struct LifecycleLayer {
    _private: (),
}

impl<S> Layer<S> for LifecycleLayer {
    type Service = Lifecycle<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Lifecycle::new(inner)
    }
}
