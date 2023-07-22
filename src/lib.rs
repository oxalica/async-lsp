//! Asynchronous [Language Server Protocol (LSP)][lsp] framework based on [tower].
//!
//! See project [README] for a general overview.
//!
//! [README]: https://github.com/oxalica/async-lsp#readme
//! [lsp]: https://microsoft.github.io/language-server-protocol/overviews/lsp/overview/
//! [tower]: https://github.com/tower-rs/tower
//!
//! This project is centered at a core service trait [`LspService`] for either Language Servers or
//! Language Clients. The main loop driver [`MainLoop`] executes the service. The additional
//! features, called middleware, are pluggable can be layered using the [`tower_layer`]
//! abstraction. This crate defines several common middlewares for various mandatory or optional
//! LSP functionalities, see their documentations for details.
//! - [`concurrency::Concurrency`]: Incoming request multiplexing and cancellation.
//! - [`panic::CatchUnwind`]: Turn panics into errors.
//! - [`tracing::Tracing`]: Logger spans with methods instrumenting handlers.
//! - [`server::Lifecycle`]: Server initialization, shutting down, and exit handling.
//! - [`client_monitor::ClientProcessMonitor`]: Client process monitor.
//! - [`router::Router`]: "Root" service to dispatch requests, notifications and events.
//!
//! Users are free to select and layer middlewares to run a Language Server or Language Client.
//! They can also implement their own middlewares for like timeout, metering, request
//! transformation and etc.
//!
//! ## Usages
//!
//! There are two main ways to define a [`Router`](router::Router) root service: one is via its
//! builder API, and the other is to construct via implementing the omnitrait [`LanguageServer`] or
//! [`LanguageClient`] for a state struct. The former is more flexible, while the latter has a
//! more similar API as [`tower-lsp`](https://crates.io/crates/tower-lsp).
//!
//! The examples for both builder-API and omnitrait, cross Language Server and Language Client, can
//! be seen under
#![doc = concat!("[`examples`](https://github.com/oxalica/async-lsp/tree/v", env!("CARGO_PKG_VERSION") , "/examples)")]
//! directory.
//!
//! ## Cargo features
//!
//! - `client-monitor`: Client process monitor middleware [`client_monitor`].
//!   *Enabled by default.*
//! - `omni-trait`: Mega traits of all standard requests and notifications, namely
//!   [`LanguageServer`] and [`LanguageClient`].
//!   *Enabled by default.*
//! - `stdio`: Utilities to deal with pipe-like stdin/stdout communication channel for Language
//!   Servers.
//!   *Enabled by default.*
//! - `tracing`: Integration with crate [`tracing`][::tracing] and the [`tracing`] middleware.
//!   *Enabled by default.*
//! - `forward`: Impl [`LspService`] for `{Client,Server}Socket`. This collides some method names
//!   but allows easy service forwarding. See `examples/inspector.rs` for a possible use case.
//!   *Disabled by default.*
//! - `tokio`: Enable compatible methods for [`tokio`](https://crates.io/crates/tokio) runtime.
//!   *Disabled by default.*
#![cfg_attr(docsrs_, feature(doc_cfg))]
#![warn(missing_docs)]
use std::any::{type_name, Any, TypeId};
use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::marker::PhantomData;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use std::{fmt, io};

use futures::channel::{mpsc, oneshot};
use futures::io::BufReader;
use futures::stream::FuturesUnordered;
use futures::{
    pin_mut, select_biased, AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite,
    AsyncWriteExt, FutureExt, SinkExt, StreamExt,
};
use lsp_types::notification::Notification;
use lsp_types::request::Request;
use lsp_types::NumberOrString;
use once_cell::sync::Lazy;
use pin_project_lite::pin_project;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue};
use thiserror::Error;
use tower_service::Service;

macro_rules! define_getters {
    (impl[$($generic:tt)*] $ty:ty, $field:ident : $field_ty:ty) => {
        impl<$($generic)*> $ty {
            /// Get a reference to the inner service.
            #[must_use]
            pub fn get_ref(&self) -> &$field_ty {
                &self.$field
            }

            /// Get a mutable reference to the inner service.
            #[must_use]
            pub fn get_mut(&mut self) -> &mut $field_ty {
                &mut self.$field
            }

            /// Consume self, returning the inner service.
            #[must_use]
            pub fn into_inner(self) -> $field_ty {
                self.$field
            }
        }
    };
}

pub mod concurrency;
pub mod panic;
pub mod router;
pub mod server;

#[cfg(feature = "forward")]
#[cfg_attr(docsrs_, doc(cfg(feature = "forward")))]
mod forward;

#[cfg(feature = "client-monitor")]
#[cfg_attr(docsrs_, doc(cfg(feature = "client-monitor")))]
pub mod client_monitor;

#[cfg(all(feature = "stdio", unix))]
#[cfg_attr(docsrs_, doc(cfg(all(feature = "stdio", unix))))]
pub mod stdio;

#[cfg(feature = "tracing")]
#[cfg_attr(docsrs_, doc(cfg(feature = "tracing")))]
pub mod tracing;

#[cfg(feature = "omni-trait")]
mod omni_trait;
#[cfg(feature = "omni-trait")]
#[cfg_attr(docsrs_, doc(cfg(feature = "omni-trait")))]
pub use omni_trait::{LanguageClient, LanguageServer};

/// A convenient type alias for `Result` with `E` = [`enum@crate::Error`].
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
    /// The underlying channel reached EOF (end of file).
    #[error("the underlying channel reached EOF")]
    Eof,
    /// No handlers for events or mandatory notifications (not starting with `$/`).
    ///
    /// Will not occur when catch-all handlers ([`router::Router::unhandled_event`] and
    /// [`router::Router::unhandled_notification`]) are installed.
    #[error("{0}")]
    Routing(String),
}

/// The core service abstraction, representing either a Language Server or Language Client.
pub trait LspService: Service<AnyRequest> {
    /// The handler of [LSP notifications](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#notificationMessage).
    ///
    /// Notifications are delivered in order and synchronously. This is mandatory since they can
    /// change the interpretation of later notifications or requests.
    ///
    /// # Return
    ///
    /// The return value decides the action to either break or continue the main loop.
    fn notify(&mut self, notif: AnyNotification) -> ControlFlow<Result<()>>;

    /// The handler of an arbitrary [`AnyEvent`].
    ///
    /// Events are emitted by users or middlewares via [`ClientSocket::emit`] or
    /// [`ServerSocket::emit`], for user-defined purposes. Events are delivered in order and
    /// synchronously.
    ///
    /// # Return
    ///
    /// The return value decides the action to either break or continue the main loop.
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
    /// Invalid JSON was received by the server. An error occurred on the server while parsing the
    /// JSON text.
    ///
    /// Defined by [JSON-RPC](https://www.jsonrpc.org/specification#error_object).
    pub const PARSE_ERROR: Self = Self(-32700);

    /// The JSON sent is not a valid Request object.
    ///
    /// Defined by [JSON-RPC](https://www.jsonrpc.org/specification#error_object).
    pub const INVALID_REQUEST: Self = Self(-32600);

    /// The method does not exist / is not available.
    ///
    /// Defined by [JSON-RPC](https://www.jsonrpc.org/specification#error_object).
    pub const METHOD_NOT_FOUND: Self = Self(-32601);

    /// Invalid method parameter(s).
    ///
    /// Defined by [JSON-RPC](https://www.jsonrpc.org/specification#error_object).
    pub const INVALID_PARAMS: Self = Self(-32602);

    /// Internal JSON-RPC error.
    ///
    /// Defined by [JSON-RPC](https://www.jsonrpc.org/specification#error_object).
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

    /// (Defined by LSP specification without description)
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

#[derive(Serialize)]
struct RawJsonrpc<T> {
    jsonrpc: RpcVersion,
    #[serde(flatten)]
    inner: T,
}

impl<T> RawJsonrpc<T> {
    fn new(inner: T) -> Self {
        Self {
            jsonrpc: RpcVersion::V2,
            inner,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum RpcVersion {
    #[serde(rename = "2.0")]
    V2,
}

// TODO: Unify these structs?
#[derive(Serialize)]
struct RawNotification<M, P> {
    method: M,
    params: P,
}

#[derive(Serialize)]
struct RawRequest<'a, S: ?Sized> {
    id: i32,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<&'a S>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawResponse {
    id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ResponseError>,
}

// Workaround: Deserialization is done through `IncomingMessage`, since `untagged`'s internal
// buffering breaks `RawJsonValue`.
// See: https://github.com/serde-rs/json/issues/497
// TODO: No-copy.
#[derive(Debug, Clone, Deserialize)]
struct RawIncomingMessage {
    #[allow(unused)]
    jsonrpc: RpcVersion,
    id: Option<RequestId>,
    method: Option<String>,
    params: Option<Box<RawJsonValue>>,
    result: Option<JsonValue>,
    error: Option<ResponseError>,
}

#[derive(Debug, Clone)]
enum IncomingMessage {
    Request(AnyRequest),
    Response(RawResponse),
    Notification(AnyNotification),
}

impl<'de> Deserialize<'de> for IncomingMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let msg = RawIncomingMessage::deserialize(deserializer)?;
        Ok(match (msg.method, msg.id) {
            (Some(method), None) => Self::Notification(AnyNotification {
                method,
                params: msg.params,
            }),
            (Some(method), Some(id)) => Self::Request(AnyRequest {
                id,
                method,
                params: msg.params,
            }),
            (None, Some(id)) => Self::Response(RawResponse {
                id,
                result: msg.result,
                error: msg.error,
            }),
            (None, None) => {
                return Err(D::Error::custom("invalid message without `method` or `id`"))
            }
        })
    }
}

static NULL_VALUE: Lazy<&'static RawJsonValue> =
    Lazy::new(|| serde_json::from_str("null").unwrap());

/// A dynamic runtime [LSP request](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#requestMessage).
#[derive(Debug, Clone)]
pub struct AnyRequest {
    id: RequestId,
    method: String,
    // TODO: No-copy.
    params: Option<Box<RawJsonValue>>,
}

impl AnyRequest {
    /// Get the request id.
    #[must_use]
    pub fn id(&self) -> &RequestId {
        &self.id
    }

    /// Get the method to be invoked.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Get the method's parameters in raw JSON form.
    ///
    /// If there is no parameters, a sentinel value `null` is returned. Note that according to the
    /// LSP specification, `null` is not a valid value for parameters.
    /// See: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#requestMessage
    #[must_use]
    pub fn params(&self) -> &RawJsonValue {
        self.params.as_deref().unwrap_or(*NULL_VALUE)
    }
}

/// A dynamic runtime [LSP notification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#notificationMessage).
#[derive(Debug, Clone)]
pub struct AnyNotification {
    /// The method to be invoked.
    method: String,
    /// The notification's params.
    params: Option<Box<RawJsonValue>>,
}

impl AnyNotification {
    /// Get the method to be invoked.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Get the notification's parameters in raw JSON form.
    ///
    /// If there is no parameters, a sentinel value `null` is returned. Note that according to the
    /// LSP specification, `null` is not a valid value for parameters.
    /// See: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#notificationMessage
    #[must_use]
    pub fn params(&self) -> &RawJsonValue {
        self.params.as_deref().unwrap_or(*NULL_VALUE)
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Box<RawJsonValue>>,
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
    pub fn new_with_data(
        code: ErrorCode,
        message: impl fmt::Display,
        data: &impl Serialize,
    ) -> Self {
        Self {
            code,
            message: message.to_string(),
            data: Some(serde_json::value::to_raw_value(&data).expect("failed to serialize")),
        }
    }
}

macro_rules! CONTENT_LENGTH {
    () => {
        "Content-Length"
    };
}

impl IncomingMessage {
    async fn read(mut reader: impl AsyncBufRead + Unpin) -> Result<Self> {
        let mut line = String::new();
        let mut content_len = None;
        loop {
            line.clear();
            reader.read_line(&mut line).await?;
            if line.is_empty() {
                return Err(Error::Eof);
            }
            if line == "\r\n" {
                break;
            }
            // NB. LSP spec is stricter than HTTP spec, the spaces here is required and it's not
            // explicitly permitted to include extra spaces. We reject them here.
            let (name, value) = line
                .strip_suffix("\r\n")
                .and_then(|line| line.split_once(": "))
                .ok_or_else(|| Error::Protocol(format!("Invalid header: {line:?}")))?;
            if name.eq_ignore_ascii_case(CONTENT_LENGTH!()) {
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
        ::tracing::info!(msg = %String::from_utf8_lossy(&buf), "incoming");
        Ok(serde_json::from_slice::<IncomingMessage>(&buf)?)
    }
}

/// Serialized bytes of an outgoing message with header.
struct MessageFrame {
    buf: Vec<u8>,
    start: usize,
}

impl MessageFrame {
    // Content-length has type `number`, which is defined to be `i32` and must >= 0.
    // Thus it consists of at most 10 digits.
    const HEADER: &str = concat!(CONTENT_LENGTH!(), ": 1234567890\r\n\r\n");
    const MAX_LEN_DIGITS: usize = 10;

    fn new(value: &impl Serialize) -> Self {
        use std::io::Write;

        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(Self::HEADER.as_bytes());
        serde_json::to_writer(&mut buf, &RawJsonrpc::new(value)).expect("failed to serialize");

        let len = buf.len() - Self::HEADER.len();
        let len = i32::try_from(len).expect("message length must not exceed i32::MAX");
        let start = Self::MAX_LEN_DIGITS - (1 + len.max(1).ilog10() as usize);
        let mut len_buf = &mut buf[start..Self::HEADER.len()];
        write!(len_buf, concat!(CONTENT_LENGTH!(), ": {}"), len).unwrap();
        Self { buf, start }
    }

    fn data(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    async fn write(&self, mut writer: impl AsyncWrite + Unpin) -> Result<()> {
        #[cfg(feature = "tracing")]
        ::tracing::trace!(msg = &*String::from_utf8_lossy(self.data()), "outgoing");
        writer.write_all(self.data()).await?;
        writer.flush().await?;
        Ok(())
    }
}

/// Request frame with unfilled `id`.
struct HalfRequestFrame(MessageFrame);

impl HalfRequestFrame {
    const ID_OFFSET: usize = MessageFrame::HEADER.len() + r#"{"jsonrpc":"2.0","id":"#.len();
    const PLACEHOLDER_ID: i32 = i32::MIN;
    const ID_LEN: usize = 1 + 10; // `-`

    fn new<T: Serialize + ?Sized + 'static>(method: &str, params: &T) -> Self {
        let frame = MessageFrame::new(&RawRequest {
            id: Self::PLACEHOLDER_ID,
            method,
            // XXX: Special case to skip `()` as request parameter.
            // `null` is not a valid `params` value.
            params: (TypeId::of::<T>() != TypeId::of::<()>()).then_some(params),
        });
        debug_assert_eq!(
            &frame.buf[Self::ID_OFFSET..Self::ID_OFFSET + Self::ID_LEN],
            format!("{}", Self::PLACEHOLDER_ID).as_bytes(),
            "{}",
            String::from_utf8_lossy(&frame.buf),
        );
        Self(frame)
    }

    fn finish(mut self, id: i32) -> MessageFrame {
        use std::io::Write;

        let mut len_buf = &mut self.0.buf[Self::ID_OFFSET..Self::ID_OFFSET + Self::ID_LEN];
        write!(len_buf, "{id:width$}", width = Self::ID_LEN).unwrap();
        self.0
    }
}

/// Service main loop driver for either Language Servers or Language Clients.
pub struct MainLoop<S: LspService> {
    service: S,
    rx: mpsc::UnboundedReceiver<MainLoopEvent>,
    outgoing_id: i32,
    outgoing: HashMap<i32, oneshot::Sender<RawResponse>>,
    tasks: FuturesUnordered<RequestFuture<S::Future>>,
}

enum MainLoopEvent {
    OutgoingNotification(MessageFrame),
    OutgoingRequest(HalfRequestFrame, oneshot::Sender<RawResponse>),
    Any(AnyEvent),
}

define_getters!(impl[S: LspService] MainLoop<S>, service: S);

impl<S> MainLoop<S>
where
    S: LspService<Response = JsonValue>,
    ResponseError: From<S::Error>,
{
    /// Create a Language Server main loop.
    #[must_use]
    pub fn new_server(builder: impl FnOnce(ClientSocket) -> S) -> (Self, ClientSocket) {
        let (this, socket) = Self::new(|socket| builder(ClientSocket(socket)));
        (this, ClientSocket(socket))
    }

    /// Create a Language Client main loop.
    #[must_use]
    pub fn new_client(builder: impl FnOnce(ServerSocket) -> S) -> (Self, ServerSocket) {
        let (this, socket) = Self::new(|socket| builder(ServerSocket(socket)));
        (this, ServerSocket(socket))
    }

    fn new(builder: impl FnOnce(PeerSocket) -> S) -> (Self, PeerSocket) {
        let (tx, rx) = mpsc::unbounded();
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
    /// Shortcut to [`MainLoop::run`] that accept an `impl AsyncRead` and implicit wrap it in a
    /// [`BufReader`].
    // Documented in `Self::run`.
    #[allow(clippy::missing_errors_doc)]
    pub async fn run_buffered(self, input: impl AsyncRead, output: impl AsyncWrite) -> Result<()> {
        self.run(BufReader::new(input), output).await
    }

    /// Drive the service main loop to provide the service.
    ///
    /// # Errors
    ///
    /// - `Error::Io` when the underlying `input` or `output` raises an error.
    /// - `Error::Deserialize` when the peer sends undecodable or invalid message.
    /// - `Error::Protocol` when the peer violates Language Server Protocol.
    /// - Other errors raised from service handlers.
    pub async fn run(mut self, input: impl AsyncBufRead, output: impl AsyncWrite) -> Result<()> {
        pin_mut!(input, output);
        let incoming = futures::stream::unfold(input, |mut input| async move {
            Some((IncomingMessage::read(&mut input).await, input))
        });
        let outgoing = futures::sink::unfold(output, |mut output, frame| async move {
            MessageFrame::write(&frame, &mut output)
                .await
                .map(|()| output)
        });
        pin_mut!(incoming, outgoing);

        let mut flush_fut = futures::future::Fuse::terminated();
        let ret = loop {
            // Outgoing > internal > incoming.
            // Preference on outgoing data provides back pressure in case of
            // flooding incoming requests.
            let ctl = select_biased! {
                // Concurrently flush out the previous message.
                ret = flush_fut => { ret?; continue; }

                out = self.tasks.select_next_some() => ControlFlow::Continue(Some(out)),
                event = self.rx.next() => self.dispatch_event(event.expect("Sender is alive")),
                msg = incoming.next() => {
                    let dispatch_fut = self.dispatch_message(msg.expect("Never ends")?).fuse();
                    pin_mut!(dispatch_fut);
                    // NB. Concurrently wait for `poll_ready`, and write out the last message.
                    // If the service is waiting for client's response of the last request, while
                    // the last message is not delivered on the first write, it can deadlock.
                    loop {
                        select_biased! {
                            // Dispatch first. It usually succeeds immediately for non-requests,
                            // and the service is hardly busy.
                            ctl = dispatch_fut => break ctl,
                            ret = flush_fut => { ret?; continue }
                        }
                    }
                }
            };
            let out = match ctl {
                ControlFlow::Continue(Some(out)) => out,
                ControlFlow::Continue(None) => continue,
                ControlFlow::Break(ret) => break ret,
            };
            // Flush the previous one and load a new message to send.
            outgoing.feed(out).await?;
            flush_fut = outgoing.flush().fuse();
        };

        // Flush the last message. It is enqueued before the event returning `ControlFlow::Break`.
        // To preserve the order at best effort, we send it before exiting the main loop.
        // But the more significant `ControlFlow::Break` error will override the flushing error,
        // if there is any.
        let flush_ret = outgoing.close().await;
        ret.and(flush_ret)
    }

    async fn dispatch_message(
        &mut self,
        msg: IncomingMessage,
    ) -> ControlFlow<Result<()>, Option<MessageFrame>> {
        match msg {
            IncomingMessage::Request(req) => {
                if let Err(err) = poll_fn(|cx| self.service.poll_ready(cx)).await {
                    let frame = MessageFrame::new(&RawResponse {
                        id: req.id().clone(),
                        result: None,
                        error: Some(err.into()),
                    });
                    return ControlFlow::Continue(Some(frame));
                }
                let id = req.id().clone();
                let fut = self.service.call(req);
                self.tasks.push(RequestFuture { fut, id: Some(id) });
            }
            IncomingMessage::Response(resp) => {
                if let Some(resp_tx) = match &resp.id {
                    NumberOrString::Number(id) => self.outgoing.remove(id),
                    NumberOrString::String(_) => None,
                } {
                    // The result may be ignored.
                    let _: Result<_, _> = resp_tx.send(resp);
                } else {
                    #[cfg(feature = "tracing")]
                    ::tracing::warn!(?resp, "ignored unknow response");
                }
            }
            IncomingMessage::Notification(notif) => {
                self.service.notify(notif)?;
            }
        }
        ControlFlow::Continue(None)
    }

    fn dispatch_event(
        &mut self,
        event: MainLoopEvent,
    ) -> ControlFlow<Result<()>, Option<MessageFrame>> {
        match event {
            MainLoopEvent::OutgoingRequest(half_frame, callback) => {
                let frame = half_frame.finish(self.outgoing_id);
                assert!(self.outgoing.insert(self.outgoing_id, callback).is_none());
                self.outgoing_id += 1;
                ControlFlow::Continue(Some(frame))
            }
            MainLoopEvent::OutgoingNotification(raw) => ControlFlow::Continue(Some(raw)),
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

impl<Fut, Error> Future for RequestFuture<Fut>
where
    Fut: Future<Output = Result<JsonValue, Error>>,
    ResponseError: From<Error>,
{
    type Output = MessageFrame;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let (result, error) = match ready!(this.fut.poll(cx)) {
            Ok(v) => (Some(v), None),
            Err(err) => (None, Some(err.into())),
        };
        let frame = MessageFrame::new(&RawResponse {
            id: this.id.take().expect("Future is consumed"),
            result,
            error,
        });
        Poll::Ready(frame)
    }
}

macro_rules! impl_socket_wrapper {
    ($name:ident) => {
        impl $name {
            /// Create a closed socket outside a main loop. Any interaction will immediately return
            /// an error of [`Error::ServiceStopped`].
            ///
            /// This works as a placeholder where a socket is required but actually unused.
            ///
            /// # Note
            ///
            /// To prevent accidental misusages, this method is NOT implemented as
            /// [`Default::default`] intentionally.
            #[must_use]
            pub fn new_closed() -> Self {
                Self(PeerSocket::new_closed())
            }

            /// Send a request to the peer and wait for its response.
            ///
            /// # Errors
            /// - [`Error::ServiceStopped`] when the service main loop stopped.
            /// - [`Error::Response`] when the peer replies an error.
            pub async fn request<R: Request>(&self, params: R::Params) -> Result<R::Result>
            where
                R::Params: 'static,
            {
                self.0.request::<R>(params).await
            }

            /// Send a notification to the peer and wait for its response.
            ///
            /// This is done asynchronously. An `Ok` result indicates the message is successfully
            /// queued, but may not be sent to the peer yet.
            ///
            /// # Errors
            /// - [`Error::ServiceStopped`] when the service main loop stopped.
            pub fn notify<N: Notification>(&self, params: N::Params) -> Result<()>
            where
                N::Params: 'static,
            {
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
    fn new_closed() -> Self {
        let (tx, _rx) = mpsc::unbounded();
        Self { tx }
    }

    fn send(&self, v: MainLoopEvent) -> Result<()> {
        self.tx.unbounded_send(v).map_err(|_| Error::ServiceStopped)
    }

    fn request<R: Request>(&self, params: R::Params) -> PeerSocketRequestFuture<R::Result>
    where
        R::Params: 'static,
    {
        let half_frame = HalfRequestFrame::new(R::METHOD, &params);
        let (tx, rx) = oneshot::channel();
        // If sending fails, the oneshot channel will also be closed, and it is handled by
        // `PeerSocketRequestFuture`.
        let _: Result<_, _> = self.send(MainLoopEvent::OutgoingRequest(half_frame, tx));
        PeerSocketRequestFuture {
            rx,
            _marker: PhantomData,
        }
    }

    fn notify<N: Notification>(&self, params: N::Params) -> Result<()>
    where
        N::Params: 'static,
    {
        let frame = MessageFrame::new(&RawNotification {
            method: N::METHOD,
            params,
        });
        self.send(MainLoopEvent::OutgoingNotification(frame))
    }

    pub fn emit<E: Send + 'static>(&self, event: E) -> Result<()> {
        self.send(MainLoopEvent::Any(AnyEvent::new(event)))
    }
}

struct PeerSocketRequestFuture<T> {
    rx: oneshot::Receiver<RawResponse>,
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
pub struct AnyEvent {
    inner: Box<dyn Any + Send>,
    type_name: &'static str,
}

impl fmt::Debug for AnyEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyEvent")
            .field("type_name", &self.type_name)
            .finish_non_exhaustive()
    }
}

impl AnyEvent {
    #[must_use]
    fn new<T: Send + 'static>(v: T) -> Self {
        AnyEvent {
            inner: Box::new(v),
            type_name: type_name::<T>(),
        }
    }

    #[must_use]
    fn inner_type_id(&self) -> TypeId {
        // Call `type_id` on the inner `dyn Any`, not `Box<_> as Any` or `&Box<_> as Any`.
        Any::type_id(&*self.inner)
    }

    /// Get the underlying type name for debugging purpose.
    ///
    /// The result string is only meant for debugging. It is not stable and cannot be trusted.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.type_name
    }

    /// Returns `true` if the inner type is the same as `T`.
    #[must_use]
    pub fn is<T: Send + 'static>(&self) -> bool {
        self.inner.is::<T>()
    }

    /// Returns some reference to the inner value if it is of type `T`, or `None` if it isn't.
    #[must_use]
    pub fn downcast_ref<T: Send + 'static>(&self) -> Option<&T> {
        self.inner.downcast_ref::<T>()
    }

    /// Returns some mutable reference to the inner value if it is of type `T`, or `None` if it
    /// isn't.
    #[must_use]
    pub fn downcast_mut<T: Send + 'static>(&mut self) -> Option<&mut T> {
        self.inner.downcast_mut::<T>()
    }

    /// Attempt to downcast it to a concrete type.
    ///
    /// # Errors
    ///
    /// Returns `self` if the type mismatches.
    pub fn downcast<T: Send + 'static>(self) -> Result<T, Self> {
        match self.inner.downcast::<T>() {
            Ok(v) => Ok(*v),
            Err(inner) => Err(Self {
                inner,
                type_name: self.type_name,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn _main_loop_future_is_send<S>(
        f: MainLoop<S>,
        input: impl AsyncBufRead + Send,
        output: impl AsyncWrite + Send,
    ) -> impl Send
    where
        S: LspService<Response = JsonValue> + Send,
        S::Future: Send,
        S::Error: From<Error> + Send,
        ResponseError: From<S::Error>,
    {
        f.run(input, output)
    }

    #[tokio::test]
    async fn closed_client_socket() {
        let socket = ClientSocket::new_closed();
        assert!(matches!(
            socket.notify::<lsp_types::notification::Exit>(()),
            Err(Error::ServiceStopped)
        ));
        assert!(matches!(
            socket.request::<lsp_types::request::Shutdown>(()).await,
            Err(Error::ServiceStopped)
        ));
        assert!(matches!(socket.emit(42i32), Err(Error::ServiceStopped)));
    }

    #[tokio::test]
    async fn closed_server_socket() {
        let socket = ServerSocket::new_closed();
        assert!(matches!(
            socket.notify::<lsp_types::notification::Exit>(()),
            Err(Error::ServiceStopped)
        ));
        assert!(matches!(
            socket.request::<lsp_types::request::Shutdown>(()).await,
            Err(Error::ServiceStopped)
        ));
        assert!(matches!(socket.emit(42i32), Err(Error::ServiceStopped)));
    }

    #[test]
    fn any_event() {
        #[derive(Debug, Clone, PartialEq, Eq)]
        struct MyEvent<T>(T);

        let event = MyEvent("hello".to_owned());
        let mut any_event = AnyEvent::new(event.clone());
        assert!(any_event.type_name().contains("MyEvent"));

        assert!(!any_event.is::<String>());
        assert!(!any_event.is::<MyEvent<i32>>());
        assert!(any_event.is::<MyEvent<String>>());

        assert_eq!(any_event.downcast_ref::<i32>(), None);
        assert_eq!(any_event.downcast_ref::<MyEvent<String>>(), Some(&event));

        assert_eq!(any_event.downcast_mut::<MyEvent<i32>>(), None);
        any_event.downcast_mut::<MyEvent<String>>().unwrap().0 += " world";

        let any_event = any_event.downcast::<()>().unwrap_err();
        let inner = any_event.downcast::<MyEvent<String>>().unwrap();
        assert_eq!(inner.0, "hello world");
    }

    impl MessageFrame {
        fn data_str(&self) -> &str {
            std::str::from_utf8(self.data()).unwrap()
        }
    }

    #[derive(Serialize)]
    struct Foo {
        foo: &'static str,
        bar: i32,
    }

    #[test]
    fn serialize_frame() {
        assert_eq!(
            MessageFrame::new(&Foo {
                foo: "bar",
                bar: 42,
            })
            .data_str(),
            "Content-Length: 38\r\n\r\n{\"jsonrpc\":\"2.0\",\"foo\":\"bar\",\"bar\":42}",
        );

        assert_eq!(
            MessageFrame::new(&Foo {
                foo: "this is a long string making content length two digits..",
                bar: i32::MIN,
            })
            .data_str(),
            "Content-Length: 100\r\n\r\n{\"jsonrpc\":\"2.0\",\"foo\":\"this is a long string making content length two digits..\",\"bar\":-2147483648}",
        );
    }

    #[test]
    fn serialize_request_simple() {
        let frame = HalfRequestFrame::new("method", &42).finish(626);
        assert_eq!(
            frame.data_str(),
            "Content-Length: 64\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":        626,\"method\":\"method\",\"params\":42}",
        );
    }

    #[test]
    fn serialize_request_empty() {
        let frame = HalfRequestFrame::new("shutdown", &()).finish(i32::MIN);
        assert_eq!(
            frame.data_str(),
            "Content-Length: 54\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":-2147483648,\"method\":\"shutdown\"}"
        );
    }
}
