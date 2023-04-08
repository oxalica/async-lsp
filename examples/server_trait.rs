use std::ops::ControlFlow;
use std::time::Duration;

use async_lsp::client_monitor::ClientProcessMonitorLayer;
use async_lsp::concurrency::ConcurrencyLayer;
use async_lsp::panic::CatchUnwindLayer;
use async_lsp::router::Router;
use async_lsp::server::LifecycleLayer;
use async_lsp::stdio::{PipeStdin, PipeStdout};
use async_lsp::tracing::TracingLayer;
use async_lsp::{Client, LanguageClient, LanguageServer, ResponseError};
use futures::future::BoxFuture;
use lsp_types::{
    DidChangeConfigurationParams, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverContents, HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
    MarkedString, MessageType, OneOf, ServerCapabilities, ShowMessageParams,
};
use tokio::io::BufReader;
use tower::ServiceBuilder;
use tracing::{info, Level};

struct ServerState {
    client: Client,
    counter: i32,
}

impl LanguageServer for ServerState {
    type Error = ResponseError;

    fn initialize(
        &mut self,
        params: InitializeParams,
    ) -> BoxFuture<'static, Result<InitializeResult, Self::Error>> {
        eprintln!("Initialize with {params:?}");
        Box::pin(async move {
            Ok(InitializeResult {
                capabilities: ServerCapabilities {
                    hover_provider: Some(HoverProviderCapability::Simple(true)),
                    definition_provider: Some(OneOf::Left(true)),
                    ..ServerCapabilities::default()
                },
                server_info: None,
            })
        })
    }

    fn hover(&mut self, _: HoverParams) -> BoxFuture<'static, Result<Option<Hover>, Self::Error>> {
        let client = self.client.clone();
        let counter = self.counter;
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            client
                .show_message(ShowMessageParams {
                    typ: MessageType::INFO,
                    message: "Hello LSP".into(),
                })
                .await
                .unwrap();
            Ok(Some(Hover {
                contents: HoverContents::Scalar(MarkedString::String(format!(
                    "I am a hover text {counter}!"
                ))),
                range: None,
            }))
        })
    }

    fn definition(
        &mut self,
        _: GotoDefinitionParams,
    ) -> BoxFuture<'static, Result<Option<GotoDefinitionResponse>, ResponseError>> {
        unimplemented!("Not yet implemented!");
    }

    fn did_change_configuration(
        &mut self,
        _: DidChangeConfigurationParams,
    ) -> ControlFlow<async_lsp::Result<()>> {
        ControlFlow::Continue(())
    }
}

struct TickEvent;

impl ServerState {
    fn new_router(client: Client) -> Router<Self> {
        let mut router = Router::from_language_server(Self { client, counter: 0 });
        router.event(Self::on_tick);
        router
    }

    fn on_tick(&mut self, _: TickEvent) -> ControlFlow<async_lsp::Result<()>> {
        info!("tick");
        self.counter += 1;
        ControlFlow::Continue(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let server = async_lsp::Server::new(1, |client| {
        tokio::spawn({
            let client = client.clone();
            async move {
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                loop {
                    interval.tick().await;
                    if client.emit(TickEvent).await.is_err() {
                        break;
                    };
                }
            }
        });

        ServiceBuilder::new()
            .layer(TracingLayer::default())
            .layer(LifecycleLayer)
            .layer(CatchUnwindLayer::new())
            .layer(ConcurrencyLayer::new(4))
            .layer(ClientProcessMonitorLayer::new(client.clone()))
            .service(ServerState::new_router(client))
    });

    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    let stdin = BufReader::new(PipeStdin::lock().unwrap());
    let stdout = PipeStdout::lock().unwrap();
    server.run(stdin, stdout).await.unwrap();
}
