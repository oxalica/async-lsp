use std::ops::ControlFlow;
use std::path::Path;
use std::process::Stdio;

use async_lsp::concurrency::ConcurrencyLayer;
use async_lsp::panic::CatchUnwindLayer;
use async_lsp::router::Router;
use async_lsp::tracing::TracingLayer;
use async_lsp::{LanguageClient, LanguageServer, ResponseError};
use futures::channel::oneshot;
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, HoverContents, HoverParams, InitializeParams,
    InitializedParams, MarkupContent, NumberOrString, Position, ProgressParams,
    ProgressParamsValue, PublishDiagnosticsParams, ShowMessageParams, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Url, WindowClientCapabilities, WorkDoneProgress,
    WorkDoneProgressParams, WorkspaceFolder,
};
use tower::ServiceBuilder;
use tracing::{info, Level};

const TEST_ROOT: &str = "tests/client_test_data";
// Old and new token names.
const RA_INDEXING_TOKENS: &[&str] = &["rustAnalyzer/Indexing", "rustAnalyzer/cachePriming"];

struct ClientState {
    indexed_tx: Option<oneshot::Sender<()>>,
}

impl LanguageClient for ClientState {
    type Error = ResponseError;
    type NotifyResult = ControlFlow<async_lsp::Result<()>>;

    fn progress(&mut self, params: ProgressParams) -> Self::NotifyResult {
        tracing::info!("{:?} {:?}", params.token, params.value);
        if matches!(params.token, NumberOrString::String(s) if RA_INDEXING_TOKENS.contains(&&*s))
            && matches!(
                params.value,
                ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
            )
        {
            // Sometimes rust-analyzer auto-index multiple times?
            if let Some(tx) = self.indexed_tx.take() {
                let _: Result<_, _> = tx.send(());
            }
        }
        ControlFlow::Continue(())
    }

    fn publish_diagnostics(&mut self, _: PublishDiagnosticsParams) -> Self::NotifyResult {
        ControlFlow::Continue(())
    }

    fn show_message(&mut self, params: ShowMessageParams) -> Self::NotifyResult {
        tracing::info!("Message {:?}: {}", params.typ, params.message);
        ControlFlow::Continue(())
    }
}

impl ClientState {
    fn new_router(indexed_tx: oneshot::Sender<()>) -> Router<Self> {
        let mut router = Router::from_language_client(ClientState {
            indexed_tx: Some(indexed_tx),
        });
        router.event(Self::on_stop);
        router
    }

    fn on_stop(&mut self, _: Stop) -> ControlFlow<async_lsp::Result<()>> {
        ControlFlow::Break(Ok(()))
    }
}

struct Stop;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let root_dir = Path::new(TEST_ROOT)
        .canonicalize()
        .expect("test root should be valid");

    let (indexed_tx, indexed_rx) = oneshot::channel();
    let (mainloop, mut server) = async_lsp::MainLoop::new_client(|_server| {
        ServiceBuilder::new()
            .layer(TracingLayer::default())
            .layer(CatchUnwindLayer::default())
            .layer(ConcurrencyLayer::default())
            .service(ClientState::new_router(indexed_tx))
    });

    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    let child = async_process::Command::new("rust-analyzer")
        .current_dir(&root_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed run rust-analyzer");
    let stdout = child.stdout.unwrap();
    let stdin = child.stdin.unwrap();

    let mainloop_fut = tokio::spawn(async move {
        mainloop.run_buffered(stdout, stdin).await.unwrap();
    });

    // Initialize.
    let init_ret = server
        .initialize(InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: Url::from_file_path(&root_dir).unwrap(),
                name: "root".into(),
            }]),
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
    let file_path = root_dir.join("src/lib.rs");
    let file_uri = Url::from_file_path(&file_path).unwrap();
    let file_content = std::fs::read_to_string(&file_path).unwrap();
    server
        .did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: file_uri.clone(),
                language_id: "rust".into(),
                version: 0,
                text: file_content.clone(),
            },
        })
        .unwrap();

    // Wait until indexed.
    indexed_rx.await.unwrap();

    // Query.
    let var_pos = file_content.find("var").unwrap();
    let hover = server
        .hover(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: file_uri },
                position: Position::new(0, var_pos as _),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        })
        .await
        .unwrap()
        .unwrap();
    info!("Hover result: {hover:?}");
    assert!(
        matches!(
            hover.contents,
            HoverContents::Markup(MarkupContent { value, .. })
            if value.contains("let var: i32")
        ),
        "should show the type of `var`",
    );

    // Shutdown.
    server.shutdown(()).await.unwrap();
    server.exit(()).unwrap();

    server.emit(Stop).unwrap();
    mainloop_fut.await.unwrap();
}

#[test]
#[ignore = "invokes rust-analyzer"]
fn rust_analyzer() {
    main()
}
