use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use lsp_types::notification::{self, Notification};
use pin_project_lite::pin_project;
use tokio::sync::oneshot;
use tower_layer::Layer;
use tower_service::Service;

use crate::{
    AnyEvent, AnyNotification, AnyRequest, ErrorCode, JsonValue, LspService, RequestId,
    ResponseError, Result,
};

pub struct Concurrency<S> {
    service: S,
    max_concurrency: usize,
    ongoing: HashMap<RequestId, oneshot::Sender<Infallible>>,
}

impl<S: LspService> Service<AnyRequest> for Concurrency<S> {
    type Response = JsonValue;
    type Error = ResponseError;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.ongoing.retain(|_, tx| tx.poll_closed(cx).is_pending());
        if self.ongoing.len() < self.max_concurrency {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        assert!(self.ongoing.len() <= self.max_concurrency);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.ongoing.insert(req.id.clone(), cancel_tx);
        let fut = self.service.call(req);
        ResponseFuture { fut, cancel_rx }
    }
}

pin_project! {
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

#[derive(Clone, Debug)]
#[must_use]
pub struct ConcurrencyBuilder {
    max_concurrency: usize,
}

impl ConcurrencyBuilder {
    pub fn new(max_concurrency: usize) -> Self {
        assert_ne!(max_concurrency, 0);
        Self { max_concurrency }
    }
}

pub type ConcurrencyLayer = ConcurrencyBuilder;

impl<S> Layer<S> for ConcurrencyBuilder {
    type Service = Concurrency<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Concurrency {
            service: inner,
            max_concurrency: self.max_concurrency,
            ongoing: HashMap::with_capacity(self.max_concurrency),
        }
    }
}
