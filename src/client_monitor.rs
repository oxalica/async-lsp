use std::io;
use std::ops::ControlFlow;
use std::task::{Context, Poll};

use lsp_types::request::{self, Request};
use tower_layer::Layer;
use tower_service::Service;

use crate::{
    AnyEvent, AnyNotification, AnyRequest, ClientSocket, Error, JsonValue, LspService,
    ResponseError, Result,
};

struct ClientProcessExited;

pub struct ClientProcessMonitor<S> {
    service: S,
    client: ClientSocket,
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
                match wait_for_pid(pid).await {
                    Ok(()) => {
                        // Ignore channel close.
                        let _: Result<_, _> = client.emit(ClientProcessExited);
                    }
                    Err(_err) => {
                        #[cfg(feature = "tracing")]
                        ::tracing::error!("Failed to monitor peer process ({pid}): {_err:#}");
                    }
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

#[cfg(target_os = "linux")]
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

#[cfg(not(target_os = "linux"))]
async fn wait_for_pid(pid: i32) -> io::Result<()> {
    use std::time::Duration;

    use rustix::io::Errno;
    use rustix::process::{test_kill_process, Pid};

    // Accuracy doesn't matter.
    // This monitor is only to avoid process leakage when the peer goes wrong.
    #[cfg(not(test))]
    const POLL_PERIOD: Duration = Duration::from_secs(30);
    // But it matters in tests.
    #[cfg(test)]
    const POLL_PERIOD: Duration = Duration::from_millis(100);

    fn is_alive(pid: Pid) -> io::Result<bool> {
        match test_kill_process(pid) {
            Ok(()) => Ok(true),
            Err(Errno::SRCH) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    #[cfg(feature = "tracing")]
    ::tracing::warn!("Unsupported platform to monitor exit of non-child processes, fallback to polling with kill(2)");

    let pid = pid
        .try_into()
        .ok()
        .and_then(|pid| unsafe { Pid::from_raw(pid) })
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, format!("Invalid PID {pid}")))?;

    let mut interval = tokio::time::interval(POLL_PERIOD);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while {
        interval.tick().await;
        is_alive(pid)?
    } {}
    Ok(())
}

#[must_use]
pub struct ClientProcessMonitorBuilder {
    client: ClientSocket,
}

impl ClientProcessMonitorBuilder {
    pub fn new(client: ClientSocket) -> Self {
        Self { client }
    }
}

pub type ClientProcessMonitorLayer = ClientProcessMonitorBuilder;

impl<S> Layer<S> for ClientProcessMonitorBuilder {
    type Service = ClientProcessMonitor<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ClientProcessMonitor {
            service: inner,
            client: self.client.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;
    use std::time::Duration;

    use rustix::io::Errno;
    use rustix::process::{kill_process, waitpid, Pid, RawPid, Signal, WaitOptions};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    #[tokio::test]
    async fn wait_for_pid() {
        // Don't stuck when something goes wrong.
        const FUSE_DURATION_SEC: u32 = 10;
        const WAIT_DURATION: Duration = Duration::from_secs(1);
        // NB. Should be less than the period of poll-impl of `wait_for_pid`.
        const TOLERANCE: Duration = Duration::from_millis(200);

        let mut sh = Command::new("sh")
            .args([
                "-c",
                &format!(
                    "
                    sleep {FUSE_DURATION_SEC} &
                    echo $!
                    "
                ),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Failed to run sh");
        let nonchild_raw_pid = {
            let mut buf = String::new();
            BufReader::new(sh.stdout.as_mut().unwrap())
                .read_line(&mut buf)
                .await
                .unwrap();
            buf.trim().parse::<i32>().unwrap()
        };
        assert!(nonchild_raw_pid >= 2);

        // Grandchildren should not be children.
        #[allow(clippy::unnecessary_cast)] // Linux and macOS have different `RawPid` types.
        let nonchild_pid = unsafe { Pid::from_raw(nonchild_raw_pid as RawPid).unwrap() };
        assert_ne!(sh.id().unwrap(), nonchild_raw_pid as u32);
        assert_eq!(
            waitpid(Some(nonchild_pid), WaitOptions::NOHANG).unwrap_err(),
            Errno::CHILD
        );

        let wait_task = tokio::spawn(super::wait_for_pid(nonchild_raw_pid));

        // Wait for some time to make sure no unexpected returns.
        tokio::time::sleep(WAIT_DURATION).await;
        if wait_task.is_finished() {
            panic!("Unexpected return {:?}", wait_task.await);
        }

        // Kill the grandchild, and it should return in time.
        kill_process(nonchild_pid, Signal::Term).unwrap();
        tokio::time::timeout(TOLERANCE, wait_task)
            .await
            .expect("Should returns in time")
            .expect("Should not panic")
            .expect("Should succeeds");
    }
}
