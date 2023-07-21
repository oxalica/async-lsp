//! Attach [`tracing::Span`]s over underlying handlers.
//!
//! *Applies to both Language Servers and Language Clients.*
//!
//! This middleware attaches spans to logs in underlying implementations, with optional method
//! strings of current processing requests/notifications.
//! All of these methods are instrumented by the [`Default`] configuration:
//! - [`Service::poll_ready`].
//! - [`Future::poll`] of returned `Future` from [`Service::call`].
//! - [`LspService::notify`].
//! - [`LspService::emit`].
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tower_layer::Layer;
use tower_service::Service;
use tracing::{info_span, Span};

use crate::{AnyEvent, AnyNotification, AnyRequest, LspService, Result};

/// The middleware attaching [`tracing::Span`]s over underlying handlers.
///
/// See [module level documentations](self) for details.
#[derive(Default)]
pub struct Tracing<S> {
    service: S,
    spans: TracingBuilder,
}

define_getters!(impl[S] Tracing<S>, service: S);

impl<S: LspService> Service<AnyRequest> for Tracing<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _guard = self.spans.service_ready.map(|f| f().entered());
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        ResponseFuture {
            span: self.spans.request.map(|f| f(&req)),
            fut: self.service.call(req),
        }
    }
}

pin_project! {
    /// The [`Future`] type used by the [`Tracing`] middleware.
    pub struct ResponseFuture<Fut> {
        span: Option<Span>,
        #[pin]
        fut: Fut,
    }
}

impl<Fut: Future> Future for ResponseFuture<Fut> {
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let _guard = this.span.as_mut().map(|span| span.enter());
        this.fut.poll(cx)
    }
}

impl<S: LspService> LspService for Tracing<S> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        let _guard = self.spans.notification.map(|f| f(&notif).entered());
        self.service.notify(notif)
    }

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
        let _guard = self.spans.event.map(|f| f(&event).entered());
        self.service.emit(event)
    }
}

/// The builder of [`Tracing`] middleware.
///
/// See [module level documentations](self) for details.
#[derive(Clone)]
#[must_use]
pub struct TracingBuilder {
    service_ready: Option<fn() -> Span>,
    request: Option<fn(&AnyRequest) -> Span>,
    notification: Option<fn(&AnyNotification) -> Span>,
    event: Option<fn(&AnyEvent) -> Span>,
}

impl Default for TracingBuilder {
    fn default() -> Self {
        Self {
            service_ready: Some(|| info_span!("service_ready")),
            request: Some(|req| info_span!("request", method = req.method())),
            notification: Some(|notif| info_span!("notification", method = notif.method())),
            event: Some(|event| info_span!("event", type_name = event.type_name())),
        }
    }
}

impl TracingBuilder {
    /// Creating the builder with no spans configured.
    ///
    /// NB. This is **NOT** the same as [`TracingBuilder::default`] which configures ALL default
    /// spans.
    pub fn new() -> Self {
        Self {
            service_ready: None,
            request: None,
            notification: None,
            event: None,
        }
    }

    /// Set a [`tracing::Span`] builder to instrument [`Service::poll_ready`] method.
    pub fn service_ready(mut self, f: fn() -> Span) -> Self {
        self.service_ready = Some(f);
        self
    }

    /// Set a [`tracing::Span`] builder to instrument [`Future::poll`] of the `Future` returned by
    /// [`Service::call`].
    pub fn request(mut self, f: fn(&AnyRequest) -> Span) -> Self {
        self.request = Some(f);
        self
    }

    /// Set a [`tracing::Span`] builder to instrument [`LspService::notify`].
    pub fn notification(mut self, f: fn(&AnyNotification) -> Span) -> Self {
        self.notification = Some(f);
        self
    }

    /// Set a [`tracing::Span`] builder to instrument [`LspService::emit`].
    pub fn event(mut self, f: fn(&AnyEvent) -> Span) -> Self {
        self.event = Some(f);
        self
    }

    /// Build the middleware with the current configuration.
    pub fn build<S>(&self, service: S) -> Tracing<S> {
        Tracing {
            service,
            spans: self.clone(),
        }
    }
}

/// A type alias of [`TracingLayer`] conforming to the naming convention of [`tower_layer`].
pub type TracingLayer = TracingBuilder;

impl<S> Layer<S> for TracingBuilder {
    type Service = Tracing<S>;

    fn layer(&self, inner: S) -> Self::Service {
        self.build(inner)
    }
}
