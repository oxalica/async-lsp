use std::any::{type_name, Any, TypeId};
use std::collections::HashMap;
use std::future::poll_fn;
use std::ops::ControlFlow;
use std::pin::pin;
use std::{fmt, io};

use lsp_types::notification::Notification;
use lsp_types::request::Request;
use lsp_types::NumberOrString;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tower_service::Service;

pub mod concurrency;
pub mod monitor;
pub mod panic;
pub mod router;
pub mod server;

#[cfg(all(feature = "stdio", unix))]
pub mod stdio;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("Channel closed")]
    ChannelClosed,
    #[error("Deserialization failed: {0}")]
    Deserialize(#[from] serde_json::Error),
    #[error("Error response {}: {}", .0.code, .0.message)]
    Response(ResponseError),
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
}

pub trait LspService: Service<AnyRequest, Response = JsonValue, Error = ResponseError> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>>;

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnyRequest {
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ErrorCode(pub i32);

impl From<i32> for ErrorCode {
    fn from(i: i32) -> Self {
        Self(i)
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ErrorCode {
    // Defined by JSON-RPC
    pub const PARSE_ERROR: Self = Self(-32700);
    pub const INVALID_REQUEST: Self = Self(-32600);
    pub const METHOD_NOT_FOUND: Self = Self(-32601);
    pub const INVALID_PARAMS: Self = Self(-32602);
    pub const INTERNAL_ERROR: Self = Self(-32603);

    /// This is the start range of JSON-RPC reserved error codes.
    /// It doesn't denote a real error code. No LSP error codes should
    /// be defined between the start and end range. For backwards
    /// compatibility the `ServerNotInitialized` and the `UnknownErrorCode`
    /// are left in the range.
    ///
    /// @since 3.16.0
    pub const JSONRPC_RESERVED_ERROR_RANGE_START: Self = Self(-32099);

    /// Error code indicating that a server received a notification or
    /// request before the server has received the `initialize` request.
    pub const SERVER_NOT_INITIALIZED: Self = Self(-32002);
    pub const UNKNOWN_ERROR_CODE: Self = Self(-32001);

    /// This is the end range of JSON-RPC reserved error codes.
    /// It doesn't denote a real error code.
    ///
    /// @since 3.16.0
    pub const JSONRPC_RESERVED_ERROR_RANGE_END: Self = Self(-32000);

    /// This is the start range of LSP reserved error codes.
    /// It doesn't denote a real error code.
    ///
    /// @since 3.16.0
    pub const LSP_RESERVED_ERROR_RANGE_START: Self = Self(-32899);

    /// A request failed but it was syntactically correct, e.g the
    /// method name was known and the parameters were valid. The error
    /// message should contain human readable information about why
    /// the request failed.
    ///
    /// @since 3.17.0
    pub const REQUEST_FAILED: Self = Self(-32803);

    /// The server cancelled the request. This error code should
    /// only be used for requests that explicitly support being
    /// server cancellable.
    ///
    /// @since 3.17.0
    pub const SERVER_CANCELLED: Self = Self(-32802);

    /// The server detected that the content of a document got
    /// modified outside normal conditions. A server should
    /// NOT send this error code if it detects a content change
    /// in it unprocessed messages. The result even computed
    /// on an older state might still be useful for the client.
    ///
    /// If a client decides that a result is not of any use anymore
    /// the client should cancel the request.
    pub const CONTENT_MODIFIED: Self = Self(-32801);

    /// The client has canceled a request and a server as detected
    /// the cancel.
    pub const REQUEST_CANCELLED: Self = Self(-32800);

    /// This is the end range of LSP reserved error codes.
    /// It doesn't denote a real error code.
    ///
    /// @since 3.16.0
    pub const LSP_RESERVED_ERROR_RANGE_END: Self = Self(-32800);
}

// Rejects `null`.
pub type RequestId = NumberOrString;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RawMessage<T> {
    jsonrpc: RpcVersion,
    #[serde(flatten)]
    inner: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum RpcVersion {
    #[serde(rename = "2.0")]
    V2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum Message {
    Request(AnyRequest),
    Response(AnyResponse),
    Notification(AnyNotification),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnyNotification {
    pub method: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub params: JsonValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnyResponse {
    id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ResponseError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Error)]
#[error("{code}: {message} (data = {data:?})")]
pub struct ResponseError {
    pub code: ErrorCode,
    pub message: String,
    pub data: Option<JsonValue>,
}

impl Message {
    const CONTENT_LENGTH: &str = "Content-Length";

    async fn read(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Self> {
        let mut line = String::new();
        let mut content_len = None;
        loop {
            line.clear();
            reader.read_line(&mut line).await?;
            if line == "\r\n" {
                break;
            }
            let (name, value) = line
                .strip_suffix("\r\n")
                .and_then(|line| line.split_once(": "))
                .ok_or_else(|| Error::Protocol(format!("Invalid header: {line:?}")))?;
            if name.eq_ignore_ascii_case(Self::CONTENT_LENGTH) {
                let value = value
                    .parse::<usize>()
                    .map_err(|_| Error::Protocol(format!("Invalid content-length: {value}")))?;
                content_len = Some(value);
            }
        }
        let content_len =
            content_len.ok_or_else(|| Error::Protocol("Missing content-length".into()))?;
        let mut buf = vec![0u8; content_len];
        reader.read_exact(&mut buf).await?;
        Ok(serde_json::from_slice(&buf)?)
    }

    async fn write(&self, writer: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
        let buf = serde_json::to_string(self)?;
        writer
            .write_all(format!("{}: {}\r\n\r\n", Self::CONTENT_LENGTH, buf.len()).as_bytes())
            .await?;
        writer.write_all(buf.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }
}

pub struct Server<S> {
    service: S,
    tx: mpsc::Sender<MainLoopEvent>,
    rx: mpsc::Receiver<MainLoopEvent>,
    outgoing_id: i32,
    outgoing: HashMap<RequestId, oneshot::Sender<AnyResponse>>,
}

enum MainLoopEvent {
    Incoming(Message),
    Outgoing(Message),
    OutgoingRequest(AnyRequest, oneshot::Sender<AnyResponse>),
    Any(AnyEvent),
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

    pub async fn run(
        mut self,
        input: impl AsyncBufRead + Send + 'static,
        output: impl AsyncWrite + Send + 'static,
    ) -> Result<()> {
        // The strong reference is kept in the reader thread, so the it stops the main loop when
        // error occurs.
        let weak_tx = self.tx.downgrade();
        let reader_tx = self.tx;

        // TODO: Poll in the same task.
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        tokio::spawn(async move {
            let mut input = pin!(input);
            loop {
                let msg = Message::read(&mut input)
                    .await
                    .expect("Failed to read message");
                if reader_tx.send(MainLoopEvent::Incoming(msg)).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            let mut output = pin!(output);
            while let Some(msg) = writer_rx.recv().await {
                Message::write(&msg, &mut output)
                    .await
                    .expect("Failed to write message");
            }
        });

        while let Some(event) = self.rx.recv().await {
            match event {
                MainLoopEvent::Incoming(Message::Request(req)) => {
                    if let Err(err) = poll_fn(|cx| self.service.poll_ready(cx)).await {
                        let resp = AnyResponse {
                            id: req.id,
                            result: None,
                            error: Some(err),
                        };
                        writer_tx
                            .send(Message::Response(resp))
                            .await
                            .map_err(|_| Error::ChannelClosed)?;
                        continue;
                    }

                    let id = req.id.clone();
                    let fut = self.service.call(req);
                    let weak_tx = weak_tx.clone();
                    tokio::spawn(async move {
                        let resp = match fut.await {
                            Ok(v) => AnyResponse {
                                id,
                                result: Some(v),
                                error: None,
                            },
                            Err(err) => AnyResponse {
                                id,
                                result: None,
                                error: Some(err),
                            },
                        };
                        if let Some(tx) = weak_tx.upgrade() {
                            // If the channel closed, the error already propagates to the main
                            // loop.
                            let _: Result<_, _> = tx
                                .send(MainLoopEvent::Outgoing(Message::Response(resp)))
                                .await;
                        }
                    });
                }
                MainLoopEvent::Incoming(Message::Response(resp)) => {
                    if let Some(resp_tx) = self.outgoing.remove(&resp.id) {
                        // The result may be ignored.
                        let _: Result<_, _> = resp_tx.send(resp);
                    }
                }
                MainLoopEvent::Incoming(Message::Notification(notif)) => {
                    if let ControlFlow::Break(b) = self.service.notify(notif) {
                        return b;
                    }
                }
                MainLoopEvent::OutgoingRequest(mut req, resp_tx) => {
                    req.id = RequestId::Number(self.outgoing_id);
                    assert!(self.outgoing.insert(req.id.clone(), resp_tx).is_none());
                    self.outgoing_id += 1;
                    writer_tx
                        .send(Message::Request(req))
                        .await
                        .map_err(|_| Error::ChannelClosed)?;
                }
                MainLoopEvent::Outgoing(msg) => {
                    writer_tx
                        .send(msg)
                        .await
                        .map_err(|_| Error::ChannelClosed)?;
                }
                MainLoopEvent::Any(event) => {
                    if let ControlFlow::Break(b) = self.service.emit(event) {
                        return b;
                    }
                }
            }
        }

        Err(Error::ChannelClosed)
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    tx: mpsc::WeakSender<MainLoopEvent>,
}

impl Client {
    async fn send(&self, v: MainLoopEvent) -> Result<()> {
        self.tx
            .upgrade()
            .ok_or(Error::ChannelClosed)?
            .send(v)
            .await
            .map_err(|_| Error::ChannelClosed)
    }

    pub async fn notify<N: Notification>(&self, params: N::Params) -> Result<()> {
        let notif = AnyNotification {
            method: N::METHOD.into(),
            params: serde_json::to_value(params).expect("Failed to serialize"),
        };
        self.send(MainLoopEvent::Outgoing(Message::Notification(notif)))
            .await
    }

    pub async fn request<R: Request>(&self, params: R::Params) -> Result<R::Result> {
        let (tx, rx) = oneshot::channel();
        let req = AnyRequest {
            id: RequestId::Number(0),
            method: R::METHOD.into(),
            params: serde_json::to_value(params).expect("Failed to serialize"),
        };
        self.send(MainLoopEvent::OutgoingRequest(req, tx)).await?;
        let resp = rx.await.map_err(|_| Error::ChannelClosed)?;
        match resp.error {
            None => Ok(serde_json::from_value(resp.result.unwrap_or_default())?),
            Some(err) => Err(Error::Response(err)),
        }
    }

    pub async fn emit<E: Send + 'static>(&self, event: E) -> Result<()> {
        self.send(MainLoopEvent::Any(AnyEvent::new(event))).await
    }
}

pub struct AnyEvent(Box<dyn Any + Send>, &'static str);

impl fmt::Debug for AnyEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyEvent")
            .field("type", &self.1)
            .finish_non_exhaustive()
    }
}

impl AnyEvent {
    fn new<T: Send + 'static>(v: T) -> Self {
        AnyEvent(Box::new(v), type_name::<T>())
    }

    fn inner_type_id(&self) -> TypeId {
        // Call `type_id` on the inner `dyn Any`, not `Box<_> as Any` or `&Box<_> as Any`.
        Any::type_id(&*self.0)
    }

    pub fn downcast<T: Send + 'static>(self) -> Result<T, Self> {
        match self.0.downcast::<T>() {
            Ok(v) => Ok(*v),
            Err(b) => Err(Self(b, self.1)),
        }
    }
}
