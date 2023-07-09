//! Incoming request multiplexing limits and cancellation.
//!
//! *Applies to both Language Servers and Language Clients.*
//!
//! Note that the [`Frontend`][crate::Frontend] main loop can poll multiple ongoing requests
//! out-of-box, while this middleware is to provides these additional features:
//! 1. Limit concurrent incoming requests to at most `max_concurrency`.
//! 2. Cancellation of incoming requests via client notification `$/cancelRequest`.
use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::thread::available_parallelism;

use futures::stream::{AbortHandle, Abortable};
use futures::task::AtomicWaker;
use lsp_types::notification::{self, Notification};
use pin_project_lite::pin_project;
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
    /// A specialized single-acquire-multiple-release semaphore, using `Arc::weak_count` as tokens.
    semaphore: Arc<AtomicWaker>,
    ongoing: HashMap<RequestId, AbortHandle>,
}

define_getters!(impl[S] Concurrency<S>, service: S);

impl<S: LspService> Service<AnyRequest> for Concurrency<S> {
    type Response = JsonValue;
    type Error = ResponseError;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if Arc::weak_count(&self.semaphore) >= self.max_concurrency.get() {
            // Implicit `Acquire`.
            self.semaphore.register(cx.waker());
            // No guards dropped between the check and register?
            if Arc::weak_count(&self.semaphore) >= self.max_concurrency.get() {
                return Poll::Pending;
            }
        }

        // Here we have `weak_count < max_concurrency`. The service is ready for new calls.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        let guard = SemaphoreGuard(Arc::downgrade(&self.semaphore));
        debug_assert!(
            Arc::weak_count(&self.semaphore) <= self.max_concurrency.get(),
            "`poll_ready` is not called before `call`",
        );

        let (handle, reg) = AbortHandle::new_pair();

        // Regularly purge completed or dead tasks. See also `AbortOnDrop` below.
        // This costs 2*N time to remove at least N tasks, results in amortized O(1) time cost
        // for each spawned task.
        if self.ongoing.len() >= self.max_concurrency.get() * 2 {
            self.ongoing.retain(|_, handle| !handle.is_aborted());
        }
        self.ongoing.insert(req.id.clone(), handle.clone());

        let fut = self.service.call(req);
        let fut = Abortable::new(fut, reg);
        ResponseFuture {
            fut,
            _abort_on_drop: AbortOnDrop(handle),
            _guard: guard,
        }
    }
}

struct SemaphoreGuard(Weak<AtomicWaker>);

impl Drop for SemaphoreGuard {
    fn drop(&mut self) {
        if let Some(sema) = self.0.upgrade() {
            if let Some(waker) = sema.take() {
                // Return the token first.
                drop(sema);
                // Wake up `poll_ready`. Implicit "Release".
                waker.wake()
            }
        }
    }
}

/// By default, the `AbortHandle` only transfers information from it to `Abortable<_>`, not in
/// reverse. But we want to set the flag on drop (either success or failure), so that the `ongoing`
/// map can be purged regularly without bloating indefinitely.
struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pin_project! {
    /// The [`Future`] type used by the [`Concurrency`] middleware.
    pub struct ResponseFuture<Fut> {
        #[pin]
        fut: Abortable<Fut>,
        // NB. Comes before `SemaphoreGuard`. So that when the guard wake up the caller, it is able
        // to purge the current future from `ongoing` map immediately.
        _abort_on_drop: AbortOnDrop,
        _guard: SemaphoreGuard,
    }
}

impl<Fut: Future<Output = Result<JsonValue, ResponseError>>> Future for ResponseFuture<Fut> {
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().fut.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(inner_ret)) => Poll::Ready(inner_ret),
            Poll::Ready(Err(_aborted)) => Poll::Ready(Err(ResponseError {
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
            semaphore: Arc::new(AtomicWaker::new()),
            // See `Concurrency::call` for why the factor 2.
            ongoing: HashMap::with_capacity(
                self.max_concurrency
                    .get()
                    .checked_mul(2)
                    .expect("max_concurrency overflow"),
            ),
        }
    }
}
