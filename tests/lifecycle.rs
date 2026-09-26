use std::ops::ControlFlow;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use async_lsp::router::Router;
use async_lsp::server::LifecycleLayer;
use async_lsp::{Error, ErrorCode, MainLoop, Result, ServerSocket};
use futures::io::AsyncReadExt;
use lsp_types::{
    notification, request, ConfigurationParams, DidOpenTextDocumentParams, InitializeResult,
    InitializedParams, TextDocumentItem, Url,
};
use tokio::task::JoinHandle;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tower::ServiceBuilder;

const MEMORY_CHANNEL_SIZE: usize = 64 << 10;

#[derive(Clone, Copy)]
enum ExitBehavior {
    Continue,
    BreakOk,
    BreakErr,
}

type MainLoopTask = JoinHandle<Result<()>>;

#[derive(Default)]
struct NotificationCounts {
    initialized: AtomicUsize,
    did_open: AtomicUsize,
}

type TestSession = (ServerSocket, Arc<NotificationCounts>, MainLoopTask, MainLoopTask);

fn start(exit_behavior: ExitBehavior) -> TestSession {
    let counts = Arc::new(NotificationCounts::default());
    let initialized_counts = counts.clone();
    let did_open_counts = counts.clone();
    let (server_main, server_socket) = MainLoop::new_server(move |_client| {
        let mut router = Router::new(());
        router
            .request::<request::Initialize, _>(|_, _| async { Ok(InitializeResult::default()) })
            .request::<request::WorkspaceConfiguration, _>(|_, _| async { Ok(Vec::new()) })
            .request::<request::Shutdown, _>(|_, _| async { Ok(()) })
            .notification::<notification::Initialized>(move |_, _| {
                initialized_counts
                    .initialized
                    .fetch_add(1, Ordering::SeqCst);
                ControlFlow::Continue(())
            })
            .notification::<notification::DidOpenTextDocument>(move |_, _| {
                did_open_counts.did_open.fetch_add(1, Ordering::SeqCst);
                ControlFlow::Continue(())
            })
            .notification::<notification::Exit>(move |_, _| match exit_behavior {
                ExitBehavior::Continue => ControlFlow::Continue(()),
                ExitBehavior::BreakOk => ControlFlow::Break(Ok(())),
                ExitBehavior::BreakErr => {
                    ControlFlow::Break(Err(Error::Routing("inner exit result".into())))
                }
            });
        ServiceBuilder::new()
            .layer(LifecycleLayer::default())
            .service(router)
    });
    let (client_main, client) =
        MainLoop::new_client(|_| ServiceBuilder::new().service(Router::new(())));
    let (server_stream, client_stream) = tokio::io::duplex(MEMORY_CHANNEL_SIZE);
    let (server_rx, server_tx) = server_stream.compat().split();
    let server_task = tokio::spawn(async move {
        let _server_socket = server_socket;
        server_main.run_buffered(server_rx, server_tx).await
    });
    let (client_rx, client_tx) = client_stream.compat().split();
    let client_task = tokio::spawn(client_main.run_buffered(client_rx, client_tx));
    (client, counts, server_task, client_task)
}

async fn initialize(client: &ServerSocket) {
    assert_eq!(
        client
            .request::<request::Initialize>(Default::default())
            .await
            .unwrap(),
        InitializeResult::default()
    );
    client
        .notify::<notification::Initialized>(InitializedParams {})
        .unwrap();
}

fn did_open(client: &ServerSocket) {
    client
        .notify::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: Url::parse("file:///test.txt").unwrap(),
                language_id: "text".into(),
                version: 1,
                text: String::new(),
            },
        })
        .unwrap();
}

async fn assert_client_eof(client_task: MainLoopTask) {
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), client_task)
            .await
            .expect("client did not stop")
            .unwrap(),
        Err(Error::Eof)
    ));
}

async fn server_result(server_task: MainLoopTask) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), server_task)
        .await
        .expect("server did not stop")
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_filters_messages_until_initialized() {
    let (client, counts, server_task, client_task) = start(ExitBehavior::Continue);

    let error = client
        .request::<request::WorkspaceConfiguration>(ConfigurationParams { items: Vec::new() })
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Response(response) if response.code == ErrorCode::SERVER_NOT_INITIALIZED)
    );
    did_open(&client);
    client
        .request::<request::Initialize>(Default::default())
        .await
        .unwrap();
    assert_eq!(counts.did_open.load(Ordering::SeqCst), 0);

    let error = client
        .request::<request::Initialize>(Default::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Response(response) if response.code == ErrorCode::INVALID_REQUEST)
    );
    did_open(&client);
    let error = client
        .request::<request::WorkspaceConfiguration>(ConfigurationParams { items: Vec::new() })
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Response(response) if response.code == ErrorCode::SERVER_NOT_INITIALIZED)
    );
    assert_eq!(counts.did_open.load(Ordering::SeqCst), 0);

    client
        .notify::<notification::Initialized>(InitializedParams {})
        .unwrap();
    did_open(&client);
    client
        .request::<request::WorkspaceConfiguration>(ConfigurationParams { items: Vec::new() })
        .await
        .unwrap();
    assert_eq!(counts.did_open.load(Ordering::SeqCst), 1);
    let error = client
        .request::<request::Initialize>(Default::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Response(response) if response.code == ErrorCode::INVALID_REQUEST)
    );

    client.request::<request::Shutdown>(()).await.unwrap();
    client.notify::<notification::Exit>(()).unwrap();
    server_result(server_task).await.unwrap();
    assert_client_eof(client_task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initialized_before_initialize_is_ignored() {
    let (client, counts, server_task, client_task) = start(ExitBehavior::Continue);
    client
        .notify::<notification::Initialized>(InitializedParams {})
        .unwrap();
    client
        .request::<request::Initialize>(Default::default())
        .await
        .unwrap();
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    client
        .notify::<notification::Initialized>(InitializedParams {})
        .unwrap();
    client
        .request::<request::WorkspaceConfiguration>(ConfigurationParams { items: Vec::new() })
        .await
        .unwrap();
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
    client.request::<request::Shutdown>(()).await.unwrap();
    client.notify::<notification::Exit>(()).unwrap();
    server_result(server_task).await.unwrap();
    assert_client_eof(client_task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_initialized_is_a_protocol_error() {
    let (client, _, server_task, client_task) = start(ExitBehavior::Continue);
    initialize(&client).await;
    client
        .notify::<notification::Initialized>(InitializedParams {})
        .unwrap();
    assert!(matches!(
        server_result(server_task).await,
        Err(Error::Protocol(_))
    ));
    assert_client_eof(client_task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initialized_after_shutdown_is_ignored() {
    let (client, counts, server_task, client_task) = start(ExitBehavior::Continue);
    initialize(&client).await;
    client.request::<request::Shutdown>(()).await.unwrap();
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
    client
        .notify::<notification::Initialized>(InitializedParams {})
        .unwrap();
    client.notify::<notification::Exit>(()).unwrap();
    server_result(server_task).await.unwrap();
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
    assert_client_eof(client_task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn exit_before_shutdown_uses_a_protocol_error_by_default() {
    for initialized in [false, true] {
        let (client, _, server_task, client_task) = start(ExitBehavior::Continue);
        if initialized {
            initialize(&client).await;
        }
        client.notify::<notification::Exit>(()).unwrap();
        assert!(
            matches!(server_result(server_task).await, Err(Error::Protocol(message)) if message.contains("before shutdown"))
        );
        assert_client_eof(client_task).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_rejects_later_requests_and_ignores_notifications() {
    let (client, counts, server_task, client_task) = start(ExitBehavior::Continue);
    initialize(&client).await;
    client.request::<request::Shutdown>(()).await.unwrap();

    let error = client.request::<request::Shutdown>(()).await.unwrap_err();
    assert!(
        matches!(error, Error::Response(response) if response.code == ErrorCode::INVALID_REQUEST)
    );
    let error = client
        .request::<request::WorkspaceConfiguration>(ConfigurationParams { items: Vec::new() })
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Response(response) if response.code == ErrorCode::INVALID_REQUEST)
    );
    did_open(&client);
    client.notify::<notification::Exit>(()).unwrap();
    server_result(server_task).await.unwrap();
    assert_eq!(counts.did_open.load(Ordering::SeqCst), 0);
    assert_client_eof(client_task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_exit_break_takes_precedence_over_the_lifecycle_default() {
    for shutdown in [false, true] {
        for behavior in [ExitBehavior::BreakOk, ExitBehavior::BreakErr] {
            let (client, _, server_task, client_task) = start(behavior);
            if shutdown {
                initialize(&client).await;
                client.request::<request::Shutdown>(()).await.unwrap();
            }
            client.notify::<notification::Exit>(()).unwrap();
            let result = server_result(server_task).await;
            match behavior {
                ExitBehavior::BreakOk => result.unwrap(),
                ExitBehavior::BreakErr => assert!(
                    matches!(result, Err(Error::Routing(message)) if message == "inner exit result")
                ),
                ExitBehavior::Continue => unreachable!(),
            }
            assert_client_eof(client_task).await;
        }
    }
}
