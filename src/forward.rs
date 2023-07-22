use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::channel::oneshot;
use futures::FutureExt;
use tower_service::Service;

use crate::{
    AnyEvent, AnyNotification, AnyRequest, ClientSocket, ErrorCode, HalfRequestFrame, LspService,
    MainLoopEvent, MessageFrame, PeerSocket, RawNotification, RawResponse, ResponseError, Result,
    ServerSocket,
};

pub struct PeerSocketResponseFuture {
    rx: oneshot::Receiver<RawResponse>,
}

impl Future for PeerSocketResponseFuture {
    type Output = Result<serde_json::Value, ResponseError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.rx.poll_unpin(cx) {
            Poll::Ready(Ok(resp)) => Poll::Ready(match resp.error {
                None => Ok(resp.result.unwrap_or_default()),
                Some(resp_err) => Err(resp_err),
            }),
            Poll::Ready(Err(_closed)) => Poll::Ready(Err(ResponseError::new(
                ErrorCode::INTERNAL_ERROR,
                "forwarding service stopped",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl PeerSocket {
    fn on_call(&mut self, req: AnyRequest) -> PeerSocketResponseFuture {
        let (tx, rx) = oneshot::channel();
        let half_frame = HalfRequestFrame::new(req.method(), req.params());
        let _: Result<_, _> = self.send(MainLoopEvent::OutgoingRequest(half_frame, tx));
        PeerSocketResponseFuture { rx }
    }

    fn on_notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        let out = MessageFrame::new(&RawNotification {
            method: notif.method(),
            params: notif.params(),
        });
        match self.send(MainLoopEvent::OutgoingNotification(out)) {
            Ok(()) => ControlFlow::Continue(()),
            Err(err) => ControlFlow::Break(Err(err)),
        }
    }

    fn on_emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
        match self.send(MainLoopEvent::Any(event)) {
            Ok(()) => ControlFlow::Continue(()),
            Err(err) => ControlFlow::Break(Err(err)),
        }
    }
}

macro_rules! define_socket_wrappers {
    ($($ty:ty),*) => {
        $(
        impl Service<AnyRequest> for $ty {
            type Response = serde_json::Value;
            type Error = ResponseError;
            type Future = PeerSocketResponseFuture;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                // `*Socket` has an unbounded buffer, thus is always ready.
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, req: AnyRequest) -> Self::Future {
                self.0.on_call(req)
            }
        }

        impl LspService for $ty {
            fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
                self.0.on_notify(notif)
            }

            fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
                self.0.on_emit(event)
            }
        }
        )*
    };
}

define_socket_wrappers!(ClientSocket, ServerSocket);
