//! Incoming request multiplexing limits and cancellation.
//!
//! *Applies to both Language Servers and Language Clients.*
//!
//! Note that the [`Frontend`][crate::Frontend] main loop can poll multiple ongoing requests
//! out-of-box, while this middleware is to provides these additional features:
//! 1. Limit concurrent incoming requests to at most `max_concurrency`.
//! 2. Cancellation of incoming requests via client notification `$/cancelRequest`.
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::thread::available_parallelism;

use lsp_types::notification::{self, Notification};
use pin_project_lite::pin_project;
use tokio::sync::oneshot;
use tower_layer::Layer;
use tower_service::Service;

use crate::{
    AnyEvent, AnyNotification, AnyRequest, ErrorCode, JsonValue, LspService, RequestId,
    ResponseError, Result,
};

/// The middleware for incoming request multiplexing limits and cancellation.
///
/// See [module level documentations](self) for details.
pub struct Concurrency<S> {
    service: S,
    max_concurrency: NonZeroUsize,
    ongoing: HashMap<RequestId, oneshot::Sender<Infallible>>,
}

impl<S: LspService> Service<AnyRequest> for Concurrency<S> {
    type Response = JsonValue;
    type Error = ResponseError;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.ongoing.retain(|_, tx| tx.poll_closed(cx).is_pending());
        if self.ongoing.len() < self.max_concurrency.get() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        assert!(self.ongoing.len() < self.max_concurrency.get(), "Not ready");
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.ongoing.insert(req.id.clone(), cancel_tx);
        let fut = self.service.call(req);
        ResponseFuture { fut, cancel_rx }
    }
}

pin_project! {
    /// The [`Future`] type used by the [`Concurrency`] middleware.
    pub struct ResponseFuture<Fut> {
        #[pin]
        fut: Fut,
        cancel_rx: oneshot::Receiver<Infallible>,
    }
}

impl<Fut: Future<Output = Result<JsonValue, ResponseError>>> Future for ResponseFuture<Fut> {
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if let Poll::Ready(ret) = this.fut.poll(cx) {
            return Poll::Ready(ret);
        }
        match ready!(Pin::new(this.cancel_rx).poll(cx)) {
            Ok(never) => match never {},
            Err(_) => Poll::Ready(Err(ResponseError {
                code: ErrorCode::REQUEST_CANCELLED,
                message: "Client cancelled the request".into(),
                data: None,
            })),
        }
    }
}

impl<S: LspService> LspService for Concurrency<S> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        if notif.method == notification::Cancel::METHOD {
            if let Ok(params) = serde_json::from_value::<lsp_types::CancelParams>(notif.params) {
                self.ongoing.remove(&params.id);
            }
            return ControlFlow::Continue(());
        }
        self.service.notify(notif)
    }

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
        self.service.emit(event)
    }
}

/// The builder of [`Concurrency`] middleware.
///
/// It's [`Default`] configuration has `max_concurrency` of the result of
/// [`std::thread::available_parallelism`], fallback to `1` if it fails.
///
/// See [module level documentations](self) for details.
#[derive(Clone, Debug)]
#[must_use]
pub struct ConcurrencyBuilder {
    max_concurrency: NonZeroUsize,
}

impl Default for ConcurrencyBuilder {
    fn default() -> Self {
        Self::new(available_parallelism().unwrap_or(NonZeroUsize::new(1).unwrap()))
    }
}

impl ConcurrencyBuilder {
    /// Create the middleware with concurrency limit `max_concurrency`.
    pub fn new(max_concurrency: NonZeroUsize) -> Self {
        Self { max_concurrency }
    }
}

/// A type alias of [`ConcurrencyBuilder`] conforming to the naming convention of [`tower_layer`].
pub type ConcurrencyLayer = ConcurrencyBuilder;

impl<S> Layer<S> for ConcurrencyBuilder {
    type Service = Concurrency<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Concurrency {
            service: inner,
            max_concurrency: self.max_concurrency,
            ongoing: HashMap::with_capacity(self.max_concurrency.get()),
        }
    }
}
