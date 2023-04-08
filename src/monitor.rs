use std::io;
use std::ops::ControlFlow;
use std::task::{Context, Poll};

use lsp_types::request::{self, Request};
use tower_layer::Layer;
use tower_service::Service;

use crate::{
    AnyEvent, AnyNotification, AnyRequest, Client, Error, JsonValue, LspService, ResponseError,
    Result,
};

struct ClientProcessExited;

pub struct ClientProcessMonitor<S> {
    service: S,
    client: Client,
}

impl<S> ClientProcessMonitor<S> {
    pub fn new(service: S, client: Client) -> Self {
        Self { service, client }
    }
}

impl<S: LspService> Service<AnyRequest> for ClientProcessMonitor<S> {
    type Response = JsonValue;
    type Error = ResponseError;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        if let Some(pid) = (|| -> Option<i32> {
            (req.method == request::Initialize::METHOD)
                .then_some(&req.params)?
                .as_object()?
                .get("processId")?
                .as_i64()?
                .try_into()
                .ok()
        })() {
            let client = self.client.clone();
            tokio::spawn(async move {
                if let Ok(()) = wait_for_pid(pid).await {
                    // Ignore channel close.
                    let _: Result<_, _> = client.emit(ClientProcessExited).await;
                }
            });
        }

        self.service.call(req)
    }
}

impl<S: LspService> LspService for ClientProcessMonitor<S> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>> {
        self.service.notify(notif)
    }

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>> {
        match event.downcast::<ClientProcessExited>() {
            Ok(ClientProcessExited) => {
                ControlFlow::Break(Err(Error::Protocol("Client process exited".into())))
            }
            Err(event) => self.service.emit(event),
        }
    }
}

async fn wait_for_pid(pid: i32) -> io::Result<()> {
    use rustix::io::Errno;
    use rustix::process::{pidfd_open, Pid, PidfdFlags};
    use tokio::io::unix::{AsyncFd, AsyncFdReadyGuard};

    let pid = pid
        .try_into()
        .ok()
        .and_then(|pid| unsafe { Pid::from_raw(pid) })
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, format!("Invalid PID {pid}")))?;
    let pidfd = match pidfd_open(pid, PidfdFlags::NONBLOCK) {
        Ok(pidfd) => pidfd,
        // Already exited.
        Err(Errno::SRCH) => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    let pidfd = AsyncFd::new(pidfd)?;
    let _guard: AsyncFdReadyGuard<'_, _> = pidfd.readable().await?;
    Ok(())
}

pub struct ClientProcessMonitorLayer {
    client: Client,
}

impl ClientProcessMonitorLayer {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

impl<S> Layer<S> for ClientProcessMonitorLayer {
    type Service = ClientProcessMonitor<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ClientProcessMonitor::new(inner, self.client.clone())
    }
}
