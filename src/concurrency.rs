use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use lsp_server::{ErrorCode, RequestId};
use lsp_types::notification::{self, Notification};
use lsp_types::NumberOrString;
use tokio::sync::oneshot;
use tower::{Layer, Service};

use crate::{AnyNotification, AnyRequest, JsonValue, LspService, ResponseError, Result};

pub struct Concurrency<S> {
    service: S,
    max_concurrency: usize,
    ongoing: HashMap<RequestId, oneshot::Sender<Infallible>>,
}

impl<S> Concurrency<S> {
    pub fn new(service: S, max_concurrency: usize) -> Self {
        assert_ne!(max_concurrency, 0);
        Self {
            service,
            max_concurrency,
            ongoing: HashMap::new(),
        }
    }
}

impl<S: LspService> Service<AnyRequest> for Concurrency<S>
where
    S::Future: Send + Unpin,
{
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
        let (tx, rx) = oneshot::channel();
        self.ongoing.insert(req.id.clone(), tx);
        let fut = self.service.call(req);
        ResponseFuture(fut, rx)
    }
}

pub struct ResponseFuture<Fut>(Fut, oneshot::Receiver<Infallible>);

impl<Fut> Future for ResponseFuture<Fut>
where
    Fut: Future<Output = Result<JsonValue, ResponseError>> + Unpin,
{
    type Output = Fut::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Poll::Ready(ret) = Pin::new(&mut self.0).poll(cx) {
            return Poll::Ready(ret);
        }
        match ready!(Pin::new(&mut self.1).poll(cx)) {
            Ok(never) => match never {},
            Err(_) => Poll::Ready(Err(ResponseError {
                code: ErrorCode::RequestCanceled as _,
                message: "Client cancelled the request".into(),
                data: None,
            })),
        }
    }
}

impl<S: LspService> LspService for Concurrency<S>
where
    S::Future: Send + Unpin,
{
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        if notif.method == notification::Cancel::METHOD {
            if let Ok(params) = serde_json::from_value::<lsp_types::CancelParams>(notif.params) {
                let id = match params.id {
                    NumberOrString::Number(i) => RequestId::from(i),
                    NumberOrString::String(i) => RequestId::from(i),
                };
                self.ongoing.remove(&id);
            }
            return ControlFlow::Continue(());
        }
        self.service.notify(notif)
    }
}

pub struct ConcurrencyLayer {
    max_concurrency: usize,
}

impl ConcurrencyLayer {
    pub fn new(max_concurrency: usize) -> Self {
        Self { max_concurrency }
    }
}

impl<S> Layer<S> for ConcurrencyLayer {
    type Service = Concurrency<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Concurrency::new(inner, self.max_concurrency)
    }
}
