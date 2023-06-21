//! Stop the main loop when the Language Client process aborted unexpectedly.
//!
//! *Only applies to Language Servers.*
//!
//! Typically, the Language Client should send
//! [`exit` notification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#exit)
//! to inform the Language Server to exit. While in case of it unexpected aborted without the
//! notification, and the communicate channel is left open somehow (eg. an FIFO or UNIX domain
//! socket), the Language Server would wait indefinitely, wasting system resources.
//!
//! It is encouraged by
//! [LSP specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#initializeParams)
//! that,
//! > If the parent process (`processId`) is not alive then the server should exit (see exit
//! > notification) its process.
//!
//! And this middleware does exactly this monitor mechanism.
//!
//! Implementation:
//! - On Linux, it uses [`pidfd_open(2)`](https://man7.org/linux/man-pages/man2/pidfd_open.2.html)
//!   which is supported since Linux 5.3.
//! - On other platforms, it perioidically calls
//!   [`kill(2)`](https://man7.org/linux/man-pages/man2/kill.2.html) with zero `sig` to test the
//!   validity of the PID, without actually sending signals. The checking period is currently 30s
//!   but should not be relied on.
//!
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

/// The middleware stopping the main loop when the Language Client process aborted unexpectedly.
///
/// See [module level documentations](self) for details.
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
            let ret = std::thread::Builder::new()
                .name("client-process-monitor".into())
                .spawn(move || {
                    #[cfg(target_os = "linux")]
                    use wait_for_pid_pidfd as wait_for_pid;

                    #[cfg(all(unix, not(target_os = "linux")))]
                    use wait_for_pid_kill as wait_for_pid;

                    match wait_for_pid(pid) {
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

            // Unused without `tracing`.
            #[allow(unused_variables)]
            if let Err(err) = ret {
                #[cfg(feature = "tracing")]
                ::tracing::error!("Failed to spawn client process monitor thread: {err:#}");
            }
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

/// The builder of [`ClientProcessMonitor`] middleware.
#[must_use]
pub struct ClientProcessMonitorBuilder {
    client: ClientSocket,
}

impl ClientProcessMonitorBuilder {
    /// Create the middleware builder with a given [`ClientSocket`] to inject exit events.
    pub fn new(client: ClientSocket) -> Self {
        Self { client }
    }
}

/// A type alias of [`ClientProcessMonitorBuilder`] conforming to the naming convention of
/// [`tower_layer`].
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

#[cfg(target_os = "linux")]
fn wait_for_pid_pidfd(pid: i32) -> io::Result<()> {
    use rustix::io::{poll, retry_on_intr, Errno, PollFd, PollFlags};
    use rustix::process::{pidfd_open, Pid, PidfdFlags};

    let pid = pid
        .try_into()
        .ok()
        .and_then(|pid| unsafe { Pid::from_raw(pid) })
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, format!("Invalid PID {pid}")))?;
    let pidfd = match pidfd_open(pid, PidfdFlags::empty()) {
        Ok(pidfd) => pidfd,
        // Already exited.
        Err(Errno::SRCH) => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let mut fds = [PollFd::new(&pidfd, PollFlags::IN)];
    retry_on_intr(|| poll(&mut fds, -1 /* Infinite wait */))?;
    Ok(())
}

#[cfg(all(unix, any(test, not(target_os = "linux"))))]
fn wait_for_pid_kill(pid: i32) -> io::Result<()> {
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
    while is_alive(pid)? {
        std::thread::sleep(POLL_PERIOD);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use rustix::io::Errno;
    use rustix::process::{kill_process, waitpid, Pid, RawPid, Signal, WaitOptions};

    #[cfg(target_os = "linux")]
    #[test]
    fn wait_for_pid_pidfd() {
        run_test(super::wait_for_pid_pidfd);
    }

    #[test]
    fn wait_for_pid_kill() {
        run_test(super::wait_for_pid_kill);
    }

    fn run_test(wait_for_pid: fn(i32) -> std::io::Result<()>) {
        // Don't stuck when something goes wrong.
        const FUSE_DURATION_SEC: u32 = 10;
        const WAIT_DURATION: Duration = Duration::from_secs(1);
        // NB. Should be greater than `POLL_PERIOD`.
        const TOLERANCE: Duration = Duration::from_millis(250);

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
                .unwrap();
            buf.trim().parse::<i32>().unwrap()
        };
        assert!(nonchild_raw_pid >= 2);

        // Grandchildren should not be children.
        #[allow(clippy::unnecessary_cast)] // Linux and macOS have different `RawPid` types.
        let nonchild_pid = unsafe { Pid::from_raw(nonchild_raw_pid as RawPid).unwrap() };
        assert_ne!(sh.id(), nonchild_raw_pid as u32);
        assert_eq!(
            waitpid(Some(nonchild_pid), WaitOptions::NOHANG).unwrap_err(),
            Errno::CHILD
        );

        let wait_task = thread::spawn(move || wait_for_pid(nonchild_raw_pid));

        // Wait for some time to make sure no unexpected returns.
        thread::sleep(WAIT_DURATION);
        if wait_task.is_finished() {
            panic!(
                "Returned while the process still alive: {:?}",
                wait_task.join(),
            );
        }

        // Kill the grandchild, and it should return in time.
        kill_process(nonchild_pid, Signal::Term).unwrap();
        let inst = Instant::now();
        wait_task
            .join()
            .expect("must not panic")
            .expect("must succeed");
        let elapsed = inst.elapsed();
        assert!(elapsed < TOLERANCE, "Wait for too long? {elapsed:?}");
    }
}
