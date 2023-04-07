use std::any::{type_name, Any, TypeId};
use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::ops::ControlFlow;
use std::pin::{pin, Pin};
use std::task::{ready, Context, Poll};
use std::{fmt, io};

use futures::stream::FuturesUnordered;
use futures::{select_biased, FutureExt, StreamExt};
use lsp_types::notification::Notification;
use lsp_types::request::Request;
use lsp_types::NumberOrString;
use pin_project_lite::pin_project;
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

#[cfg(feature = "omni-trait")]
mod omni_trait;
#[cfg(feature = "omni-trait")]
pub use omni_trait::{LanguageClient, LanguageServer};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("service is stopped")]
    ServiceStopped,
    #[error("deserialization failed: {0}")]
    Deserialize(#[from] serde_json::Error),
    #[error("{0}")]
    Response(ResponseError),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("{0}")]
    Io(#[from] io::Error),
}

pub trait LspService: Service<AnyRequest, Response = JsonValue, Error = ResponseError> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>>;

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>>;
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Error,
)]
#[error("jsonrpc error {0}")]
pub struct ErrorCode(pub i32);

impl From<i32> for ErrorCode {
    fn from(i: i32) -> Self {
        Self(i)
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnyRequest {
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
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
#[error("{message} ({code})")]
pub struct ResponseError {
    pub code: ErrorCode,
    pub message: String,
    pub data: Option<JsonValue>,
}

impl Message {
    const CONTENT_LENGTH: &str = "Content-Length";

    async fn read(mut reader: impl AsyncBufRead + Unpin) -> Result<Self> {
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

    async fn write(&self, mut writer: impl AsyncWrite + Unpin) -> Result<()> {
        let buf = serde_json::to_string(self)?;
        writer
            .write_all(format!("{}: {}\r\n\r\n", Self::CONTENT_LENGTH, buf.len()).as_bytes())
            .await?;
        writer.write_all(buf.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }
}

pub struct Server<S: LspService> {
    service: S,
    rx: mpsc::Receiver<MainLoopEvent>,
    outgoing_id: i32,
    outgoing: HashMap<RequestId, oneshot::Sender<AnyResponse>>,
    tasks: FuturesUnordered<RequestFuture<S::Future>>,
}

enum MainLoopEvent {
    Outgoing(Message),
    OutgoingRequest(AnyRequest, oneshot::Sender<AnyResponse>),
    Any(AnyEvent),
}

impl<S: LspService> Server<S> {
    pub fn new(channel_size: usize, builder: impl FnOnce(Client) -> S) -> Self {
        let (tx, rx) = mpsc::channel(channel_size);
        let client = Client { tx };
        let state = builder(client);
        Self {
            service: state,
            rx,
            outgoing_id: 0,
            outgoing: HashMap::new(),
            tasks: FuturesUnordered::new(),
        }
    }

    pub async fn run(mut self, input: impl AsyncBufRead, output: impl AsyncWrite) -> Result<()> {
        let input = pin!(input);
        let mut input = pin!(futures::stream::unfold(input, |mut input| async move {
            Some((Message::read(&mut input).await, input))
        }));
        let mut output = pin!(output);

        loop {
            let ctl = select_biased! {
                msg = input.next() => self.dispatch_message(msg.expect("Never ends")?).await,
                event = self.rx.recv().fuse() => self.dispatch_event(event.expect("Sender is alive")),
                resp = self.tasks.select_next_some() => ControlFlow::Continue(Some(Message::Response(resp))),
            };
            let msg = match ctl {
                ControlFlow::Continue(Some(msg)) => msg,
                ControlFlow::Continue(None) => continue,
                ControlFlow::Break(ret) => return ret,
            };
            // TODO: Async write.
            Message::write(&msg, &mut output).await?;
        }
    }

    async fn dispatch_message(&mut self, msg: Message) -> ControlFlow<Result<()>, Option<Message>> {
        match msg {
            Message::Request(req) => {
                if let Err(err) = poll_fn(|cx| self.service.poll_ready(cx)).await {
                    let resp = AnyResponse {
                        id: req.id,
                        result: None,
                        error: Some(err),
                    };
                    return ControlFlow::Continue(Some(Message::Response(resp)));
                }
                let id = req.id.clone();
                let fut = self.service.call(req);
                self.tasks.push(RequestFuture { fut, id: Some(id) });
            }
            Message::Response(resp) => {
                if let Some(resp_tx) = self.outgoing.remove(&resp.id) {
                    // The result may be ignored.
                    let _: Result<_, _> = resp_tx.send(resp);
                }
            }
            Message::Notification(notif) => {
                self.service.notify(notif)?;
            }
        }
        ControlFlow::Continue(None)
    }

    fn dispatch_event(&mut self, event: MainLoopEvent) -> ControlFlow<Result<()>, Option<Message>> {
        match event {
            MainLoopEvent::OutgoingRequest(mut req, resp_tx) => {
                req.id = RequestId::Number(self.outgoing_id);
                assert!(self.outgoing.insert(req.id.clone(), resp_tx).is_none());
                self.outgoing_id += 1;
                ControlFlow::Continue(Some(Message::Request(req)))
            }
            MainLoopEvent::Outgoing(msg) => ControlFlow::Continue(Some(msg)),
            MainLoopEvent::Any(event) => {
                self.service.emit(event)?;
                ControlFlow::Continue(None)
            }
        }
    }
}

pin_project! {
    struct RequestFuture<Fut> {
        #[pin]
        fut: Fut,
        id: Option<RequestId>,
    }
}

impl<Fut: Future<Output = Result<JsonValue, ResponseError>>> Future for RequestFuture<Fut> {
    type Output = AnyResponse;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let (mut result, mut error) = (None, None);
        match ready!(this.fut.poll(cx)) {
            Ok(v) => result = Some(v),
            Err(err) => error = Some(err),
        }
        Poll::Ready(AnyResponse {
            id: this.id.take().expect("Future is consumed"),
            result,
            error,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    tx: mpsc::Sender<MainLoopEvent>,
}

impl Client {
    async fn send(&self, v: MainLoopEvent) -> Result<()> {
        self.tx.send(v).await.map_err(|_| Error::ServiceStopped)
    }

    pub async fn notify<N: Notification>(&self, params: N::Params) -> Result<()> {
        let notif = AnyNotification {
            method: N::METHOD.into(),
            params: serde_json::to_value(params).expect("Failed to serialize"),
        };
        self.notify_any(notif).await
    }

    async fn notify_any(&self, notif: AnyNotification) -> Result<()> {
        self.send(MainLoopEvent::Outgoing(Message::Notification(notif)))
            .await
    }

    pub async fn request<R: Request>(&self, params: R::Params) -> Result<R::Result> {
        let req = AnyRequest {
            id: RequestId::Number(0),
            method: R::METHOD.into(),
            params: serde_json::to_value(params).expect("Failed to serialize"),
        };
        let ret = self.request_any(req).await?;
        Ok(serde_json::from_value(ret)?)
    }

    async fn request_any(&self, req: AnyRequest) -> Result<JsonValue> {
        let (tx, rx) = oneshot::channel();
        self.send(MainLoopEvent::OutgoingRequest(req, tx)).await?;
        let resp = rx.await.map_err(|_| Error::ServiceStopped)?;
        match resp.error {
            None => Ok(resp.result.unwrap_or_default()),
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
