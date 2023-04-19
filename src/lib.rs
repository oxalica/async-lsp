//! Asynchronous [Language Server Protocol (LSP)][lsp] framework based on [tower].
//!
//! See project [README] for a general overview.
//!
//! [README]: https://github.com/oxalica/async-lsp#readme
//! [lsp]: https://microsoft.github.io/language-server-protocol/overviews/lsp/overview/
//! [tower]: https://github.com/tower-rs/tower
//!
//! TODO
use std::any::{type_name, Any, TypeId};
use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::marker::PhantomData;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::{fmt, io};

use futures::stream::FuturesUnordered;
use futures::{pin_mut, select_biased, FutureExt, StreamExt};
use lsp_types::notification::Notification;
use lsp_types::request::Request;
use lsp_types::NumberOrString;
use pin_project_lite::pin_project;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tower_service::Service;

pub mod concurrency;
pub mod panic;
pub mod router;
pub mod server;

#[cfg(feature = "client-monitor")]
pub mod client_monitor;

#[cfg(all(feature = "stdio", unix))]
pub mod stdio;

#[cfg(feature = "tracing")]
pub mod tracing;

#[cfg(feature = "omni-trait")]
mod omni_trait;
#[cfg(feature = "omni-trait")]
pub use omni_trait::{LanguageClient, LanguageServer};

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Possible errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The service main loop stopped.
    #[error("service stopped")]
    ServiceStopped,
    /// The peer replies undecodable or invalid responses.
    #[error("deserialization failed: {0}")]
    Deserialize(#[from] serde_json::Error),
    /// The peer replies an error.
    #[error("{0}")]
    Response(#[from] ResponseError),
    /// The peer violates the Language Server Protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// Input/output errors from the underlying channels.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// No handlers for events or mandatory notifications (not starting with `$/`).
    ///
    /// Will not occur when catch-all handlers ([`router::Router::unhandled_event`] and
    /// [`router::Router::unhandled_notification`]) are installed.
    #[error("{0}")]
    Routing(String),
}

pub trait LspService: Service<AnyRequest, Response = JsonValue, Error = ResponseError> {
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>>;

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<()>>;
}

/// A JSON-RPC error code.
///
/// Codes defined and/or used by LSP are defined as associated constants, eg.
/// [`ErrorCode::REQUEST_FAILED`].
///
/// See:
/// <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#errorCodes>
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

/// The identifier of requests and responses.
///
/// Though `null` is technically a valid id for responses, we reject it since it hardly makes sense
/// for valid communication.
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

/// A dynamic runtime request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AnyRequest {
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
}

/// A dynamic runtime notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AnyNotification {
    pub method: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub params: JsonValue,
}

/// A dynamic runtime response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
struct AnyResponse {
    id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ResponseError>,
}

/// The error object in case a request fails.
///
/// See:
/// <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#responseError>
#[derive(Debug, Clone, Serialize, Deserialize, Error)]
#[non_exhaustive]
#[error("{message} ({code})")]
pub struct ResponseError {
    /// A number indicating the error type that occurred.
    pub code: ErrorCode,
    /// A string providing a short description of the error.
    pub message: String,
    /// A primitive or structured value that contains additional
    /// information about the error. Can be omitted.
    pub data: Option<JsonValue>,
}

impl ResponseError {
    /// Create a new error object with a JSON-RPC error code and a message.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl fmt::Display) -> Self {
        Self {
            code,
            message: message.to_string(),
            data: None,
        }
    }

    /// Create a new error object with a JSON-RPC error code, a message, and any additional data.
    #[must_use]
    pub fn new_with_data(code: ErrorCode, message: impl fmt::Display, data: JsonValue) -> Self {
        Self {
            code,
            message: message.to_string(),
            data: Some(data),
        }
    }
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
        #[cfg(feature = "tracing")]
        ::tracing::trace!(msg = &*String::from_utf8_lossy(&buf), "incoming");
        Ok(serde_json::from_slice(&buf)?)
    }

    async fn write(&self, mut writer: impl AsyncWrite + Unpin) -> Result<()> {
        let buf = serde_json::to_string(self)?;
        #[cfg(feature = "tracing")]
        ::tracing::trace!(msg = buf, "outgoing");
        writer
            .write_all(format!("{}: {}\r\n\r\n", Self::CONTENT_LENGTH, buf.len()).as_bytes())
            .await?;
        writer.write_all(buf.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }
}

/// Service main loop driver for either Language Servers or Language Clients.
pub struct Frontend<S: LspService> {
    service: S,
    rx: mpsc::UnboundedReceiver<MainLoopEvent>,
    outgoing_id: i32,
    outgoing: HashMap<RequestId, oneshot::Sender<AnyResponse>>,
    tasks: FuturesUnordered<RequestFuture<S::Future>>,
}

enum MainLoopEvent {
    Outgoing(Message),
    OutgoingRequest(AnyRequest, oneshot::Sender<AnyResponse>),
    Any(AnyEvent),
}

impl<S: LspService> Frontend<S> {
    /// Create a Language Server `Frontend`.
    #[must_use]
    pub fn new_server(builder: impl FnOnce(ClientSocket) -> S) -> (Self, ClientSocket) {
        let (this, socket) = Self::new(|socket| builder(ClientSocket(socket)));
        (this, ClientSocket(socket))
    }

    /// Create a Language Client `Frontend`.
    #[must_use]
    pub fn new_client(builder: impl FnOnce(ServerSocket) -> S) -> (Self, ServerSocket) {
        let (this, socket) = Self::new(|socket| builder(ServerSocket(socket)));
        (this, ServerSocket(socket))
    }

    fn new(builder: impl FnOnce(PeerSocket) -> S) -> (Self, PeerSocket) {
        let (tx, rx) = mpsc::unbounded_channel();
        let socket = PeerSocket { tx };
        let this = Self {
            service: builder(socket.clone()),
            rx,
            outgoing_id: 0,
            outgoing: HashMap::new(),
            tasks: FuturesUnordered::new(),
        };
        (this, socket)
    }

    /// Drive the service main loop to provide the service.
    ///
    /// # Errors
    /// - `Error::Io` when the underlying `input` or `output` raises an error.
    /// - `Error::Deserialize` when the peer sends undecodable or invalid message.
    /// - `Error::Protocol` when the peer violates Language Server Protocol.
    /// - Other errors raised from service handlers.
    pub async fn run(mut self, input: impl AsyncBufRead, output: impl AsyncWrite) -> Result<()> {
        pin_mut!(input, output);
        let input = futures::stream::unfold(input, |mut input| async move {
            Some((Message::read(&mut input).await, input))
        });
        pin_mut!(input);

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

macro_rules! impl_socket_wrapper {
    ($name:ident) => {
        impl $name {
            /// Send a request to the peer and wait for its response.
            ///
            /// # Errors
            /// - [`Error::ServiceStopped`] when the service main loop stopped.
            /// - [`Error::Response`] when the peer replies an error.
            pub async fn request<R: Request>(&self, params: R::Params) -> Result<R::Result> {
                self.0.request::<R>(params).await
            }

            /// Send a notification to the peer and wait for its response.
            ///
            /// This is done asynchronously. An `Ok` result indicates the message is successfully
            /// queued, but may not be sent to the peer yet.
            ///
            /// # Errors
            /// - [`Error::ServiceStopped`] when the service main loop stopped.
            pub fn notify<N: Notification>(&self, params: N::Params) -> Result<()> {
                self.0.notify::<N>(params)
            }

            /// Emit an arbitrary loopback event object to the service handler.
            ///
            /// This is done asynchronously. An `Ok` result indicates the message is successfully
            /// queued, but may not be processed yet.
            ///
            /// # Errors
            /// - [`Error::ServiceStopped`] when the service main loop stopped.
            pub fn emit<E: Send + 'static>(&self, event: E) -> Result<()> {
                self.0.emit::<E>(event)
            }
        }
    };
}

/// The socket for Language Server to communicate with the Language Client peer.
#[derive(Debug, Clone)]
pub struct ClientSocket(PeerSocket);
impl_socket_wrapper!(ClientSocket);

/// The socket for Language Client to communicate with the Language Server peer.
#[derive(Debug, Clone)]
pub struct ServerSocket(PeerSocket);
impl_socket_wrapper!(ServerSocket);

#[derive(Debug, Clone)]
struct PeerSocket {
    tx: mpsc::UnboundedSender<MainLoopEvent>,
}

impl PeerSocket {
    fn send(&self, v: MainLoopEvent) -> Result<()> {
        self.tx.send(v).map_err(|_| Error::ServiceStopped)
    }

    fn request<R: Request>(&self, params: R::Params) -> PeerSocketRequestFuture<R::Result> {
        let req = AnyRequest {
            id: RequestId::Number(0),
            method: R::METHOD.into(),
            params: serde_json::to_value(params).expect("Failed to serialize"),
        };
        let (tx, rx) = oneshot::channel();
        // If this fails, the oneshot channel will also be closed, and it is handled by
        // `PeerSocketRequestFuture`.
        let _: Result<_, _> = self.send(MainLoopEvent::OutgoingRequest(req, tx));
        PeerSocketRequestFuture {
            rx,
            _marker: PhantomData,
        }
    }

    fn notify<N: Notification>(&self, params: N::Params) -> Result<()> {
        let notif = AnyNotification {
            method: N::METHOD.into(),
            params: serde_json::to_value(params).expect("Failed to serialize"),
        };
        self.send(MainLoopEvent::Outgoing(Message::Notification(notif)))
    }

    pub fn emit<E: Send + 'static>(&self, event: E) -> Result<()> {
        self.send(MainLoopEvent::Any(AnyEvent::new(event)))
    }
}

struct PeerSocketRequestFuture<T> {
    rx: oneshot::Receiver<AnyResponse>,
    _marker: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> Future for PeerSocketRequestFuture<T> {
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let resp = ready!(Pin::new(&mut self.rx)
            .poll(cx)
            .map_err(|_| Error::ServiceStopped))?;
        Poll::Ready(match resp.error {
            None => Ok(serde_json::from_value(resp.result.unwrap_or_default())?),
            Some(err) => Err(Error::Response(err)),
        })
    }
}

/// A dynamic runtime event.
///
/// This is a wrapper of `Box<dyn Any + Send>`, but saves the underlying type name for better
/// `Debug` impl.
pub struct AnyEvent(Box<dyn Any + Send>, &'static str);

impl fmt::Debug for AnyEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyEvent")
            .field("type", &self.1)
            .finish_non_exhaustive()
    }
}

impl AnyEvent {
    #[must_use]
    fn new<T: Send + 'static>(v: T) -> Self {
        AnyEvent(Box::new(v), type_name::<T>())
    }

    #[must_use]
    fn inner_type_id(&self) -> TypeId {
        // Call `type_id` on the inner `dyn Any`, not `Box<_> as Any` or `&Box<_> as Any`.
        Any::type_id(&*self.0)
    }

    /// Get the underlying type name for debugging purpose.
    ///
    /// The result string is only meant for debugging. It is not stable and cannot be trusted.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.1
    }

    /// Attempt to downcast it to a concrete type.
    ///
    /// # Errors
    /// Returns `self` if the type mismatches.
    pub fn downcast<T: Send + 'static>(self) -> Result<T, Self> {
        match self.0.downcast::<T>() {
            Ok(v) => Ok(*v),
            Err(b) => Err(Self(b, self.1)),
        }
    }
}
