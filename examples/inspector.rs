use std::ops::ControlFlow;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use async_lsp::{AnyEvent, AnyNotification, AnyRequest, LspService, MainLoop};
use async_process::Command;
use futures::Future;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tower_service::Service;
use tracing::Level;

struct Forward<S>(Option<S>);

impl<S: LspService> Service<AnyRequest> for Forward<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.as_mut().unwrap().poll_ready(cx)
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        self.0.as_mut().unwrap().call(req)
    }
}

impl<S: LspService> LspService for Forward<S> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<async_lsp::Result<()>> {
        self.0.as_mut().unwrap().notify(notif)
    }

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<async_lsp::Result<()>> {
        self.0.as_mut().unwrap().emit(event)
    }
}

struct Inspect<S> {
    service: S,
    incoming: &'static str,
    outgoing: &'static str,
}

impl<S: LspService> Service<AnyRequest> for Inspect<S>
where
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        tracing::info!("{} request {}", self.incoming, req.method);
        let method = req.method.clone();
        let fut = self.service.call(req);
        let outgoing = self.outgoing;
        Box::pin(async move {
            let ret = fut.await;
            tracing::info!(
                "{} response {}: {}",
                outgoing,
                method,
                if ret.is_ok() { "ok" } else { "err" },
            );
            ret
        })
    }
}

impl<S: LspService> LspService for Inspect<S>
where
    S::Future: Send + 'static,
{
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<async_lsp::Result<()>> {
        tracing::info!("{} notification {}", self.incoming, notif.method);
        self.service.notify(notif)
    }

    fn emit(&mut self, _event: AnyEvent) -> ControlFlow<async_lsp::Result<()>> {
        unreachable!()
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    let server_path = std::env::args()
        .nth(1)
        .expect("expect argument to the forwarded LSP server");
    let mut child = Command::new(server_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn");

    // Mock client to communicate with the server. Incoming messages are forwarded to stdin/out.
    let (mut mock_client, server_socket) = MainLoop::new_client(|_| Inspect {
        service: Forward(None),
        incoming: "<",
        outgoing: ">",
    });

    // Mock server to communicate with the client. Incoming messages are forwarded to child LSP.
    let (mock_server, client_socket) = MainLoop::new_server(|_| Inspect {
        service: server_socket,
        incoming: ">",
        outgoing: "<",
    });

    // Link to form a bidirectional connection.
    mock_client.get_mut().service.0 = Some(client_socket);

    let child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();
    let main1 = tokio::spawn(mock_client.run_bufferred(child_stdout, child_stdin));

    let stdin = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();
    let main2 = tokio::spawn(mock_server.run_bufferred(stdin, stdout));

    let ret = tokio::select! {
        ret = main1 => ret,
        ret = main2 => ret,
    };
    ret.expect("join error").unwrap();
}
