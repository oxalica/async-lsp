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
//! Implementation: See crate [`waitpid_any`].
use std::ops::ControlFlow;
use std::task::{Context, Poll};

use lsp_types::request::{self, Request};
use serde::Deserialize;
use tower_layer::Layer;
use tower_service::Service;

use crate::{AnyEvent, AnyNotification, AnyRequest, ClientSocket, Error, LspService, Result};

struct ClientProcessExited;

/// The middleware stopping the main loop when the Language Client process aborted unexpectedly.
///
/// See [module level documentations](self) for details.
pub struct ClientProcessMonitor<S> {
    service: S,
    client: ClientSocket,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    process_id: i32,
}

impl<S: LspService> Service<AnyRequest> for ClientProcessMonitor<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        if let Some(InitializeParams { process_id }) = (req.method() == request::Initialize::METHOD)
            .then(|| serde_json::from_str(req.params().get()).ok())
            .flatten()
        {
            match waitpid_any::WaitHandle::open(process_id) {
                Ok(mut handle) => {
                    let client = self.client.clone();
                    let spawn_ret = std::thread::Builder::new()
                        .name("client-process-monitor".into())
                        .spawn(move || {
                            match handle.wait() {
                                Ok(()) => {
                                    // Ignore channel close.
                                    let _: Result<_, _> = client.emit(ClientProcessExited);
                                }
                                #[allow(unused_variables)]
                                Err(err) => {
                                    #[cfg(feature = "tracing")]
                                    ::tracing::error!(
                                        "Failed to monitor peer process ({process_id}): {err:#}"
                                    );
                                }
                            }
                        });
                    #[allow(unused_variables)]
                    if let Err(err) = spawn_ret {
                        #[cfg(feature = "tracing")]
                        ::tracing::error!("Failed to spawn client process monitor thread: {err:#}");
                    }
                }
                // Already exited.
                #[cfg(unix)]
                Err(err) if err.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) => {
                    // Ignore channel close.
                    let _: Result<_, _> = self.client.emit(ClientProcessExited);
                }
                #[allow(unused_variables)]
                Err(err) => {
                    #[cfg(feature = "tracing")]
                    ::tracing::error!("Failed to monitor peer process {process_id}: {err:#}");
                }
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
