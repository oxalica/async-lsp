//! Provides routing for LSP requests, notifications, and events.
//!
//! Key components:
//!
//! - `LspPicker`: Trait for custom service selection.
//! - `FirstComeFirstServe`: Basic `LspPicker` implementation.
//! - `LspSteer`: Manages multiple LSP services using a picker.
//!
//! Enables flexible routing for LSP operations with customizable service selection.

use crate::can_handle::CanHandle;
use crate::{AnyEvent, AnyNotification, AnyRequest, LspService, Result};
use std::ops::ControlFlow;
use std::task::{Context, Poll};
use tower_service::Service;

/// A trait for implementing custom service selection strategies in an LSP router.
///
/// This trait allows you to define how requests, notifications, and events
/// should be distributed among multiple LSP services.
pub trait LspPicker<S> {
    /// Selects a service to handle a given request.
    ///
    /// # Arguments
    ///
    /// * `req` - The request to be handled.
    /// * `services` - A slice of available services.
    ///
    /// # Returns
    ///
    /// The index of the selected service in the `services` slice.
    fn pick(&mut self, req: &AnyRequest, services: &[S]) -> usize;

    /// Selects a service to handle a given notification.
    ///
    /// # Arguments
    ///
    /// * `notif` - The notification to be handled.
    /// * `services` - A slice of available services.
    ///
    /// # Returns
    ///
    /// The index of the selected service in the `services` slice.
    fn pick_notification(&mut self, notif: &AnyNotification, services: &[S]) -> usize;

    /// Selects a service to handle a given event.
    ///
    /// # Arguments
    ///
    /// * `event` - The event to be handled.
    /// * `services` - A slice of available services.
    ///
    /// # Returns
    ///
    /// The index of the selected service in the `services` slice.
    fn pick_event(&mut self, event: &AnyEvent, services: &[S]) -> usize;
}

impl<S, FN, FE, F> LspPicker<S> for (FN, FE, F)
where
    F: Fn(&AnyRequest, &[S]) -> usize,
    FN: Fn(&AnyNotification, &[S]) -> usize,
    FE: Fn(&AnyEvent, &[S]) -> usize,
{
    fn pick_notification(&mut self, notif: &AnyNotification, services: &[S]) -> usize {
        self.0(notif, services)
    }
    fn pick_event(&mut self, event: &AnyEvent, services: &[S]) -> usize {
        self.1(event, services)
    }
    fn pick(&mut self, req: &AnyRequest, services: &[S]) -> usize {
        self.2(req, services)
    }
}

/// A picker that selects the first service that can handle a given request, notification, or event.
pub struct FirstComeFirstServe;

impl<S> LspPicker<S> for FirstComeFirstServe
where
    S: CanHandle<AnyNotification> + CanHandle<AnyRequest> + CanHandle<AnyEvent>,
{
    fn pick_notification(&mut self, notif: &AnyNotification, services: &[S]) -> usize {
        services
            .iter()
            .position(|service| service.can_handle(notif))
            .unwrap_or(0)
    }

    fn pick_event(&mut self, event: &AnyEvent, services: &[S]) -> usize {
        services
            .iter()
            .position(|service| service.can_handle(event))
            .unwrap_or(0)
    }

    fn pick(&mut self, req: &AnyRequest, services: &[S]) -> usize {
        services
            .iter()
            .position(|service| service.can_handle(req))
            .unwrap_or(0)
    }
}

/// A struct that steers LSP requests, notifications, and events to the appropriate service.
///
/// `LspSteer` manages a collection of LSP services and uses a picker to determine
/// which service should handle each incoming request, notification, or event.
pub struct LspSteer<S, P> {
    /// The collection of LSP services managed by this `LspSteer`.
    services: Vec<S>,
    /// The picker used to select the appropriate service for each operation.
    picker: P,
}

impl<S, P> LspSteer<S, P>
where
    P: LspPicker<S>,
{
    /// Creates a new `LspSteer` instance.
    ///
    /// # Arguments
    ///
    /// * `services` - An iterator of LSP services to be managed by this `LspSteer`.
    /// * `picker` - The picker implementation used to select services.
    ///
    /// # Returns
    ///
    /// A new `LspSteer` instance with the provided services and picker.
    pub fn new(services: impl IntoIterator<Item = S>, picker: P) -> Self {
        Self {
            services: services.into_iter().collect(),
            picker,
        }
    }
}

impl<S, P> Service<AnyRequest> for LspSteer<S, P>
where
    S: Service<AnyRequest>,
    P: LspPicker<S>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        for service in &mut self.services {
            if service.poll_ready(cx).is_pending() {
                return Poll::Pending;
            }
        }
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        let idx = self.picker.pick(&req, &self.services);
        self.services[idx].call(req)
    }
}

impl<S, P> LspService for LspSteer<S, P>
where
    S: LspService,
    P: LspPicker<S>,
{
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        let idx = self.picker.pick_notification(&notif, &self.services);
        self.services[idx].notify(notif)
    }

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
        let idx = self.picker.pick_event(&event, &self.services);
        self.services[idx].emit(event)
    }
}

// Helper implementation to allow use of closures as LspPicker
impl<F, S> LspPicker<S> for F
where
    F: Fn(&AnyRequest, &[S]) -> usize
        + Fn(&AnyNotification, &[S]) -> usize
        + Fn(&AnyEvent, &[S]) -> usize,
{
    fn pick(&mut self, req: &AnyRequest, services: &[S]) -> usize {
        self(req, services)
    }

    fn pick_notification(&mut self, notif: &AnyNotification, services: &[S]) -> usize {
        self(notif, services)
    }

    fn pick_event(&mut self, event: &AnyEvent, services: &[S]) -> usize {
        self(event, services)
    }
}

// Implement `CanHandle` based on inner services:
impl<S, P> CanHandle<AnyRequest> for LspSteer<S, P>
where
    S: CanHandle<AnyRequest>,
    P: LspPicker<S>,
{
    fn can_handle(&self, req: &AnyRequest) -> bool {
        self.services.iter().any(|service| service.can_handle(req))
    }
}

impl<S, P> CanHandle<AnyNotification> for LspSteer<S, P>
where
    S: CanHandle<AnyNotification>,
    P: LspPicker<S>,
{
    fn can_handle(&self, notif: &AnyNotification) -> bool {
        self.services
            .iter()
            .any(|service| service.can_handle(notif))
    }
}

impl<S, P> CanHandle<AnyEvent> for LspSteer<S, P>
where
    S: CanHandle<AnyEvent>,
    P: LspPicker<S>,
{
    fn can_handle(&self, event: &AnyEvent) -> bool {
        self.services
            .iter()
            .any(|service| service.can_handle(event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{util::BoxLspService, NumberOrString};
    use std::{future::Future, pin::Pin, sync::Arc};

    struct MockService;

    impl Service<AnyRequest> for MockService {
        type Response = ();
        type Error = ();
        type Future = Pin<Box<dyn Future<Output = Result<(), ()>> + Send>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: AnyRequest) -> Self::Future {
            Box::pin(async { Ok(()) })
        }
    }

    impl CanHandle<AnyRequest> for MockService {
        fn can_handle(&self, _: &AnyRequest) -> bool {
            true
        }
    }

    impl CanHandle<AnyNotification> for MockService {
        fn can_handle(&self, _: &AnyNotification) -> bool {
            true
        }
    }

    impl CanHandle<AnyEvent> for MockService {
        fn can_handle(&self, _: &AnyEvent) -> bool {
            true
        }
    }

    impl LspService for MockService {
        fn notify(&mut self, _: AnyNotification) -> ControlFlow<Result<()>> {
            ControlFlow::Continue(())
        }

        fn emit(&mut self, _: AnyEvent) -> ControlFlow<Result<()>> {
            ControlFlow::Continue(())
        }
    }

    struct MockPicker;
    impl<T> LspPicker<T> for MockPicker {
        fn pick(&mut self, _: &AnyRequest, _: &[T]) -> usize {
            0
        }
        fn pick_notification(&mut self, _: &AnyNotification, _: &[T]) -> usize {
            0
        }
        fn pick_event(&mut self, _: &AnyEvent, _: &[T]) -> usize {
            0
        }
    }

    #[test]
    fn test_lsp_steer() {
        let services = vec![MockService, MockService, MockService];
        let mut steer = LspSteer::new(services, MockPicker {});

        // Test Service trait
        let request = AnyRequest {
            id: NumberOrString::Number(1),
            method: "test".into(),
            params: serde_json::Value::Null,
        };
        let _future = steer.call(request);

        // Since we can't use block_on, we'll just ensure the future is created
        // assert!(future.is_pending());

        // Test LspService trait
        let notification = AnyNotification {
            method: "test".into(),
            params: serde_json::Value::Null,
        };
        let _ = steer.notify(notification);

        let event = AnyEvent::new(Arc::new("test event"));
        let _ = steer.emit(event);
    }

    #[test]
    fn test_first_come_first_serve() {
        let services = vec![MockService, MockService, MockService];
        let picker = FirstComeFirstServe;
        let mut steer = LspSteer::new(services, picker);

        // Test Service trait
        let request = AnyRequest {
            id: NumberOrString::Number(1),
            method: "test".into(),
            params: serde_json::Value::Null,
        };
        let _future = steer.call(request);

        // Test LspService trait
        let notification = AnyNotification {
            method: "test".into(),
            params: serde_json::Value::Null,
        };
        let _ = steer.notify(notification);

        let event = AnyEvent::new(Arc::new("test event"));
        let _ = steer.emit(event);
    }

    #[test]
    fn test_dynamic_first_come_first_serve() {
        let services = vec![
            BoxLspService::new(MockService),
            BoxLspService::new(MockService),
            BoxLspService::new(MockService),
        ];
        let picker = FirstComeFirstServe;
        let mut steer = LspSteer::new(services, picker);

        // Test Service trait
        let request = AnyRequest {
            id: NumberOrString::Number(1),
            method: "test".into(),
            params: serde_json::Value::Null,
        };
        let _future = steer.call(request);
    }
}
