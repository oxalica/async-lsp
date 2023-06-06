//! An example for unit-testing via mocking servers and/or clients.
// TODO: Make this more egornomic. Maybe provide some test APIs?
use std::ops::ControlFlow;

use async_lsp::router::Router;
use async_lsp::server::LifecycleLayer;
use async_lsp::{ClientSocket, LanguageClient, LanguageServer};
use lsp_types::{
    notification, request, ConfigurationItem, ConfigurationParams, Hover, HoverContents,
    HoverParams, HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams,
    MarkedString, MessageType, Position, ServerCapabilities, ShowMessageParams,
    TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
};
use tokio::sync::mpsc;
use tower::ServiceBuilder;

const MEMORY_CHANNEL_SIZE: usize = 64 << 10; // 64KiB

struct ServerState {
    client: ClientSocket,
}

struct ClientState {
    msg_tx: mpsc::UnboundedSender<String>,
}

#[tokio::test(flavor = "current_thread")]
async fn mock_server_and_client() {
    // The server with handlers.
    let (server_main, mut client) = async_lsp::Frontend::new_server(|client| {
        let mut router = Router::new(ServerState { client });
        router
            .request::<request::Initialize, _>(|_st, _params| async move {
                Ok(InitializeResult {
                    capabilities: ServerCapabilities {
                        hover_provider: Some(HoverProviderCapability::Simple(true)),
                        ..ServerCapabilities::default()
                    },
                    server_info: None,
                })
            })
            .notification::<notification::Initialized>(|_, _| ControlFlow::Continue(()))
            .request::<request::Shutdown, _>(|_, _| async move { Ok(()) })
            .notification::<notification::Exit>(|_, _| ControlFlow::Break(Ok(())))
            .request::<request::HoverRequest, _>(|st, _params| {
                let mut client = st.client.clone();
                async move {
                    // Optionally interact with client.
                    let text = client
                        .configuration(ConfigurationParams {
                            items: vec![ConfigurationItem {
                                scope_uri: None,
                                section: Some("mylsp.hoverText".into()),
                            }],
                        })
                        .await
                        .ok()
                        .and_then(|ret| Some(ret[0].as_str()?.to_owned()))
                        .unwrap_or_default();

                    // Respond.
                    Ok(Some(Hover {
                        contents: HoverContents::Scalar(MarkedString::String(text)),
                        range: None,
                    }))
                }
            });

        ServiceBuilder::new()
            .layer(LifecycleLayer::default())
            .service(router)
    });

    // The client with handlers.
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    let (client_main, mut server) = async_lsp::Frontend::new_client(|_server| {
        let mut router = Router::new(ClientState { msg_tx });
        router
            .notification::<notification::ShowMessage>(|st, params| {
                st.msg_tx.send(params.message).unwrap();
                ControlFlow::Continue(())
            })
            .request::<request::WorkspaceConfiguration, _>(|_st, _params| async move {
                Ok(vec!["Some hover text".into()])
            });
        ServiceBuilder::new().service(router)
    });

    // Wire up a loopback channel between the server and the client.
    let (server_stream, client_stream) = tokio::io::duplex(MEMORY_CHANNEL_SIZE);
    let (server_rx, server_tx) = tokio::io::split(server_stream);
    let server_main =
        tokio::spawn(server_main.run(tokio::io::BufReader::new(server_rx), server_tx));
    let (client_rx, client_tx) = tokio::io::split(client_stream);
    let client_main =
        tokio::spawn(client_main.run(tokio::io::BufReader::new(client_rx), client_tx));

    // Send requests to the server on behalf of the client, via `ServerSocket`. It interacts with
    // the client main loop to finalize and send the request through the channel.
    server
        .initialize(InitializeParams::default())
        .await
        .unwrap();
    // Send notifications. Note that notifications are delivered asynchronously, but in order.
    server.initialized(InitializedParams {}).unwrap();

    // After the initialization sequence, do some real requests.
    let ret = server
        .hover(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new("file:///foo".parse().unwrap()),
                position: Position::new(0, 0),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        ret,
        Some(Hover {
            contents: HoverContents::Scalar(MarkedString::String("Some hover text".into())),
            range: None
        })
    );

    // In contrast, send notifications to the client on behalf of the server, via `ClientSocket`.
    client
        .show_message(ShowMessageParams {
            typ: MessageType::INFO,
            message: "Some message".into(),
        })
        .unwrap();
    // Here the client may not get notification delivered yet. Wait for it.
    assert_eq!(msg_rx.recv().await.unwrap(), "Some message");

    // Shutdown the server.
    server.shutdown(()).await.unwrap();
    server.exit(()).unwrap();

    // Now the server main loop should stop normaly.
    server_main.await.expect("no panic").expect("exit normally");
    // And the client main loop should stop due to EOF.
    let err = client_main
        .await
        .expect("no panic")
        .expect_err("should fail");
    assert!(
        matches!(err, async_lsp::Error::Eof),
        "should fail due to EOF: {err}"
    );
}
