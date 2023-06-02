use std::env::current_dir;
use std::ops::ControlFlow;
use std::process::Stdio;

use async_lsp::concurrency::ConcurrencyLayer;
use async_lsp::panic::CatchUnwindLayer;
use async_lsp::router::Router;
use async_lsp::tracing::TracingLayer;
use async_lsp::LanguageServer;
use lsp_types::notification::{Progress, PublishDiagnostics, ShowMessage};
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, HoverParams, InitializeParams,
    InitializedParams, NumberOrString, Position, ProgressParamsValue, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Url, WindowClientCapabilities, WorkDoneProgress,
    WorkDoneProgressParams,
};
use tokio::io::BufReader;
use tokio::sync::oneshot;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tower::ServiceBuilder;
use tracing::{info, Level};

struct ClientState {
    indexed_tx: Option<oneshot::Sender<()>>,
}

struct Stop;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (indexed_tx, indexed_rx) = oneshot::channel();

    let (frontend, mut server) = async_lsp::Frontend::new_client(|_server| {
        let mut router = Router::new(ClientState {
            indexed_tx: Some(indexed_tx),
        });
        router
            .notification::<Progress>(|this, prog| {
                tracing::info!("{:?} {:?}", prog.token, prog.value);
                if matches!(prog.token, NumberOrString::String(s) if s == "rustAnalyzer/Indexing")
                    && matches!(
                        prog.value,
                        ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
                    )
                {
                    let _: Result<_, _> = this.indexed_tx.take().unwrap().send(());
                }
                ControlFlow::Continue(())
            })
            .notification::<PublishDiagnostics>(|_, _| ControlFlow::Continue(()))
            .notification::<ShowMessage>(|_, params| {
                tracing::info!("Message {:?}: {}", params.typ, params.message);
                ControlFlow::Continue(())
            })
            .event(|_, _: Stop| ControlFlow::Break(Ok(())));

        ServiceBuilder::new()
            .layer(TracingLayer::default())
            .layer(CatchUnwindLayer::default())
            .layer(ConcurrencyLayer::default())
            .service(router)
    });

    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    let child = tokio::process::Command::new("rust-analyzer")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed run rust-analyzer");
    let stdout = TokioAsyncReadCompatExt::compat(BufReader::new(child.stdout.unwrap()));
    let stdin = TokioAsyncWriteCompatExt::compat_write(child.stdin.unwrap());

    let frontend_fut = tokio::spawn(async move {
        frontend.run(stdout, stdin).await.unwrap();
    });

    let root_dir = current_dir()
        .and_then(|path| path.canonicalize())
        .expect("Invalid CWD");
    let root_uri = Url::from_file_path(&root_dir).unwrap();

    // Initialize.
    let init_ret = server
        .initialize(InitializeParams {
            root_uri: Some(root_uri),
            capabilities: ClientCapabilities {
                window: Some(WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..WindowClientCapabilities::default()
                }),
                ..ClientCapabilities::default()
            },
            ..InitializeParams::default()
        })
        .await
        .unwrap();
    info!("Initialized: {init_ret:?}");
    server.initialized(InitializedParams {}).unwrap();

    // Synchronize documents.
    let file_uri = Url::from_file_path(root_dir.join("src/lib.rs")).unwrap();
    let text = "fn func() { let var = 1; }";
    server
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: file_uri.clone(),
                language_id: "rust".into(),
                version: 0,
                text: text.into(),
            },
        })
        .unwrap();

    // Wait until indexed.
    indexed_rx.await.unwrap();

    // Query.
    let var_pos = text.find("var").unwrap();
    let hover_ret = server
        .hover(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: file_uri },
                position: Position::new(0, var_pos as _),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        })
        .await
        .unwrap();
    info!("Hover result: {hover_ret:?}");

    // Shutdown.
    server.shutdown(()).await.unwrap();
    server.exit(()).unwrap();

    server.emit(Stop).unwrap();
    frontend_fut.await.unwrap();
}
