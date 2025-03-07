//! Utility types and traits for LSP services.
//!
//! This module provides trait definitions and implementations for boxed LSP services,
//! as well as helper types for working with futures in the context of LSP.
use crate::can_handle::CanHandle;
use crate::{AnyEvent, AnyNotification, AnyRequest, LspService, Result};
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower_service::Service;

/// A boxed `Future + Send` trait object.
pub type BoxFuture<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

/// Trait combining `LspService`, `CanHandle`, and `Service`, used for type erasure.
pub trait LspServiceWithCanHandle<Response: 'static, Error: 'static>:
    LspService
    + CanHandle<AnyRequest>
    + CanHandle<AnyNotification>
    + CanHandle<AnyEvent>
    + Service<AnyRequest, Future = BoxFuture<Response, Error>>
where
    Self::Future: Send + 'static,
{
}

impl<T, Response: 'static, Error: 'static> LspServiceWithCanHandle<Response, Error> for T where
    T: LspService
        + CanHandle<AnyRequest>
        + CanHandle<AnyNotification>
        + CanHandle<AnyEvent>
        + Service<AnyRequest, Future = BoxFuture<Response, Error>>
{
}

/// A boxed `Service + LspService + Send` trait object.
pub struct BoxLspService<Response, Error> {
    inner: Box<
        dyn LspServiceWithCanHandle<Response, Error, Response = Response, Error = Error> + Send,
    >,
}

impl<Response: 'static, Error: 'static> BoxLspService<Response, Error> {
    /// Creates a new `BoxLspService` with the given inner service.
    ///
    /// # Arguments
    ///
    /// * `inner` - The inner service to be boxed.
    ///
    /// # Type Parameters
    ///
    /// * `S` - The type of the inner service, which must implement `LspServiceWithCanHandle<Response, Error>`,
    ///         be `Send`, and have a `'static` lifetime.
    pub fn new<S>(inner: S) -> Self
    where
        S: LspServiceWithCanHandle<Response, Error, Response = Response, Error = Error>
            + Send
            + 'static,
    {
        BoxLspService {
            inner: Box::new(inner),
        }
    }
}

impl<R, E> Service<AnyRequest> for BoxLspService<R, E> {
    type Response = R;
    type Error = E;
    type Future = BoxFuture<R, E>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: AnyRequest) -> Self::Future {
        self.inner.call(request)
    }
}

impl<R, E> LspService for BoxLspService<R, E> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        self.inner.notify(notif)
    }

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
        self.inner.emit(event)
    }
}

impl<R, E> CanHandle<AnyRequest> for BoxLspService<R, E> {
    fn can_handle(&self, req: &AnyRequest) -> bool {
        self.inner.can_handle(req)
    }
}

impl<R, E> CanHandle<AnyNotification> for BoxLspService<R, E> {
    fn can_handle(&self, notif: &AnyNotification) -> bool {
        self.inner.can_handle(notif)
    }
}

impl<R, E> CanHandle<AnyEvent> for BoxLspService<R, E> {
    fn can_handle(&self, event: &AnyEvent) -> bool {
        self.inner.can_handle(event)
    }
}

impl<R, E> std::fmt::Debug for BoxLspService<R, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxLspService").finish()
    }
}
