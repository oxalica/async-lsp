use std::ops::ControlFlow;
use std::time::Duration;

use async_lsp::concurrency::ConcurrencyLayer;
use async_lsp::monitor::ClientProcessMonitorLayer;
use async_lsp::panic::CatchUnwindLayer;
use async_lsp::router::Router;
use async_lsp::server::LifecycleLayer;
use async_lsp::stdio::{PipeStdin, PipeStdout};
use async_lsp::{Client, LanguageClient, LanguageServer, LanguageServerSnapshot, ResponseError};
use async_trait::async_trait;
use futures::future::BoxFuture;
use lsp_types::{
    DidChangeConfigurationParams, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverContents, HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
    MarkedString, MessageType, OneOf, ServerCapabilities, ShowMessageParams,
};
use tokio::io::BufReader;
use tower::ServiceBuilder;

#[derive(Debug, Clone)]
struct ServerState {
    client: Client,
    counter: i32,
}

impl LanguageServer for ServerState {
    type Error = ResponseError;
    type Snapshot = Self;

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

    fn did_change_configuration(
        &mut self,
        _: DidChangeConfigurationParams,
    ) -> ControlFlow<async_lsp::Result<()>> {
        ControlFlow::Continue(())
    }

    fn snapshot(&mut self) -> Self::Snapshot {
        self.clone()
    }
}

#[async_trait]
impl LanguageServerSnapshot for ServerState {
    type Error = ResponseError;

    async fn hover(self, _: HoverParams) -> Result<Option<Hover>, Self::Error> {
        tokio::time::sleep(Duration::from_secs(1)).await;
        self.client
            .show_message(ShowMessageParams {
                typ: MessageType::INFO,
                message: "Hello LSP".into(),
            })
            .await
            .unwrap();
        Ok(Some(Hover {
            contents: HoverContents::Scalar(MarkedString::String(format!(
                "I am a hover text {}!",
                self.counter,
            ))),
            range: None,
        }))
    }

    async fn definition(
        self,
        _: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>, ResponseError> {
        unimplemented!("Not yet implemented!");
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
            .layer(LifecycleLayer)
            .layer(CatchUnwindLayer::new())
            .layer(ConcurrencyLayer::new(4))
            .layer(ClientProcessMonitorLayer::new(client.clone()))
            .service(ServerState::new_router(client))
    });

    let stdin = BufReader::new(PipeStdin::lock().unwrap());
    let stdout = PipeStdout::lock().unwrap();
    server.run(stdin, stdout).await.unwrap();
}
