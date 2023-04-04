use std::collections::HashMap;
use std::future::poll_fn;
use std::io;
use std::ops::ControlFlow;

use lsp_server::{
    Message, Notification as AnyNotification, Request as AnyRequest, RequestId, Response,
    ResponseError,
};
use lsp_types::notification::Notification;
use lsp_types::request::Request;
use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, oneshot};
use tower_service::Service;

pub mod concurrency;
pub mod panic;
pub mod router;
pub mod server;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Channel closed")]
    ChannelClosed,
    #[error("Deserialization failed: {0}")]
    Deserialize(#[from] serde_json::Error),
    #[error("Error response {}: {}", .0.code, .0.message)]
    Response(ResponseError),
    #[error("Protocol error: {0}")]
    Protocol(String),
}

pub trait LspService: Service<AnyRequest, Response = JsonValue, Error = ResponseError> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>>;
}

pub struct Server<S> {
    service: S,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    outgoing_id: i32,
    outgoing: HashMap<RequestId, oneshot::Sender<Response>>,
}

enum Event {
    Incoming(Message),
    Outgoing(Message),
    OutgoingRequest(AnyRequest, oneshot::Sender<Response>),
}

impl<S: LspService> Server<S>
where
    S::Future: Send + 'static,
{
    pub fn new(channel_size: usize, builder: impl FnOnce(Client) -> S) -> Self {
        let (tx, rx) = mpsc::channel(channel_size);
        let client = Client { tx: tx.downgrade() };
        let state = builder(client);
        Self {
            service: state,
            rx,
            tx,
            outgoing_id: 0,
            outgoing: HashMap::new(),
        }
    }

    pub async fn run(mut self) -> Result<()> {
        // The strong reference is kept in the reader thread, so the it stops the main loop when
        // error occurs.
        let weak_tx = self.tx.downgrade();
        let reader_tx = self.tx;

        // TODO: Async read and write.
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        std::thread::Builder::new()
            .name("Reader".into())
            .spawn(move || {
                let mut stdin = io::stdin().lock();
                while let Some(msg) = Message::read(&mut stdin).expect("Failed to read message") {
                    if reader_tx.blocking_send(Event::Incoming(msg)).is_err() {
                        break;
                    }
                }
            })
            .unwrap();
        std::thread::Builder::new()
            .name("Writer".into())
            .spawn(move || {
                let mut stdout = io::stdout().lock();
                while let Some(msg) = writer_rx.blocking_recv() {
                    Message::write(msg, &mut stdout).expect("Failed to write message");
                }
            })
            .unwrap();

        while let Some(event) = self.rx.recv().await {
            match event {
                Event::Incoming(Message::Request(req)) => {
                    if let Err(err) = poll_fn(|cx| self.service.poll_ready(cx)).await {
                        let resp = Response {
                            id: req.id,
                            result: None,
                            error: Some(err),
                        };
                        writer_tx
                            .send(resp.into())
                            .await
                            .map_err(|_| Error::ChannelClosed)?;
                        continue;
                    }

                    let id = req.id.clone();
                    let fut = self.service.call(req);
                    let weak_tx = weak_tx.clone();
                    tokio::spawn(async move {
                        let resp = match fut.await {
                            Ok(v) => Response {
                                id,
                                result: Some(v),
                                error: None,
                            },
                            Err(err) => Response {
                                id,
                                result: None,
                                error: Some(err),
                            },
                        };
                        if let Some(tx) = weak_tx.upgrade() {
                            // If the channel closed, the error already propagates to the main
                            // loop.
                            let _: Result<_, _> = tx.send(Event::Outgoing(resp.into())).await;
                        }
                    });
                }
                Event::Incoming(Message::Response(resp)) => {
                    if let Some(resp_tx) = self.outgoing.remove(&resp.id) {
                        // The result may be ignored.
                        let _: Result<_, _> = resp_tx.send(resp);
                    }
                }
                Event::Incoming(Message::Notification(notif)) => {
                    if let ControlFlow::Break(b) = self.service.notify(notif) {
                        return b;
                    }
                }
                Event::OutgoingRequest(mut req, resp_tx) => {
                    req.id = RequestId::from(self.outgoing_id);
                    assert!(self.outgoing.insert(req.id.clone(), resp_tx).is_none());
                    self.outgoing_id += 1;
                    writer_tx
                        .send(req.into())
                        .await
                        .map_err(|_| Error::ChannelClosed)?;
                }
                Event::Outgoing(msg) => {
                    writer_tx
                        .send(msg)
                        .await
                        .map_err(|_| Error::ChannelClosed)?;
                }
            }
        }

        Err(Error::ChannelClosed)
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    tx: mpsc::WeakSender<Event>,
}

impl Client {
    pub async fn notify<N: Notification>(&self, params: N::Params) -> Result<()> {
        let notif = AnyNotification::new(N::METHOD.into(), params);
        self.tx
            .upgrade()
            .ok_or(Error::ChannelClosed)?
            .send(Event::Outgoing(notif.into()))
            .await
            .map_err(|_| Error::ChannelClosed)
    }

    pub async fn request<R: Request>(&self, params: R::Params) -> Result<R::Result> {
        let (tx, rx) = oneshot::channel();
        let req = AnyRequest::new(RequestId::from(0), R::METHOD.into(), params);
        self.tx
            .upgrade()
            .ok_or(Error::ChannelClosed)?
            .send(Event::OutgoingRequest(req, tx))
            .await
            .map_err(|_| Error::ChannelClosed)?;
        let resp = rx.await.map_err(|_| Error::ChannelClosed)?;
        match resp.error {
            None => Ok(serde_json::from_value(resp.result.unwrap_or_default())?),
            Some(err) => Err(Error::Response(err)),
        }
    }
}
