use std::io;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use async_lsp::{
    AnyEvent, AnyNotification, AnyRequest, ClientSocket, ErrorCode, LspService, MainLoop,
    ResponseError,
};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use futures::future::{ready, Ready};
use futures::{AsyncBufRead, AsyncRead, AsyncWrite};
use lsp_types::notification::{LogMessage, Notification};
use lsp_types::request::{Request, SemanticTokensFullRequest};
use lsp_types::{
    LogMessageParams, MessageType, PartialResultParams, SemanticTokens, SemanticTokensParams,
    SemanticTokensResult, TextDocumentIdentifier, WorkDoneProgressParams,
};
use serde_json::json;
use tower_service::Service;

criterion_group!(benches, bench);
criterion_main!(benches);

fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let notification_frame = gen_input_frame(json!({
        "jsonrpc": "2.0",
        "method": LogMessage::METHOD,
        "params": serde_json::to_value(LogMessageParams {
            typ: MessageType::LOG,
            message: "log".into(),
        }).unwrap(),
    }));

    c.bench_function("input-notification", |b| {
        b.to_async(&rt).iter_custom(|iters| {
            let mut input = RingReader::from(&*notification_frame);
            async move {
                let (mainloop, _client) = MainLoop::new_server(|_| TestService {
                    count_notifications: iters,
                });
                let fut = mainloop.run(
                    black_box(Pin::new(&mut input) as Pin<&mut dyn AsyncBufRead>),
                    futures::io::sink(),
                );

                let inst = Instant::now();
                fut.await.unwrap();
                inst.elapsed()
            }
        });
    });

    c.bench_function("output-notification", |b| {
        b.to_async(&rt).iter_custom(|iters| async move {
            let (mainloop, client) = MainLoop::new_server(|_| TestService {
                count_notifications: iters,
            });
            for _ in 0..iters {
                client
                    .notify::<LogMessage>(LogMessageParams {
                        typ: MessageType::LOG,
                        message: "log".into(),
                    })
                    .unwrap();
            }
            client.emit(()).unwrap();

            let mut output = futures::io::sink();
            let fut = mainloop.run(
                PendingReader,
                black_box(Pin::new(&mut output) as Pin<&mut dyn AsyncWrite>),
            );

            let inst = Instant::now();
            fut.await.unwrap();
            inst.elapsed()
        });
    });

    let request_frame = gen_input_frame(json!({
        "jsonrpc": "2.0",
        "method": SemanticTokensFullRequest::METHOD,
        "id": 42,
        "params": SemanticTokensParams {
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            text_document: TextDocumentIdentifier::new("untitled:Untitled-1".parse().unwrap()),
        },
    }));

    c.bench_function("request-throughput", |b| {
        b.to_async(&rt).iter_custom(|iters| {
            let mut input = RingReader::from(&*request_frame);
            async move {
                let (mainloop, client) = MainLoop::new_server(|_| TestService {
                    count_notifications: 0,
                });

                let mut output = CounterWriter {
                    limit: iters,
                    client,
                };
                let fut = mainloop.run(
                    black_box(Pin::new(&mut input) as Pin<&mut dyn AsyncBufRead>),
                    black_box(Pin::new(&mut output) as Pin<&mut dyn AsyncWrite>),
                );

                let inst = Instant::now();
                fut.await.unwrap();
                inst.elapsed()
            }
        });
    });
}

struct TestService {
    count_notifications: u64,
}

impl Service<AnyRequest> for TestService {
    type Response = serde_json::Value;
    type Error = ResponseError;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        let ret = if req.method() == SemanticTokensFullRequest::METHOD {
            Ok(
                serde_json::to_value(SemanticTokensResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: Vec::new(),
                }))
                .unwrap(),
            )
        } else {
            Err(ResponseError::new(
                ErrorCode::METHOD_NOT_FOUND,
                "method not found",
            ))
        };
        ready(ret)
    }
}

impl LspService for TestService {
    fn notify(&mut self, _notif: AnyNotification) -> ControlFlow<async_lsp::Result<()>> {
        if self.count_notifications == 0 {
            return ControlFlow::Break(Ok(()));
        }
        self.count_notifications -= 1;
        ControlFlow::Continue(())
    }

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<async_lsp::Result<()>> {
        event.downcast::<()>().unwrap();
        ControlFlow::Break(Ok(()))
    }
}

fn gen_input_frame(v: serde_json::Value) -> Vec<u8> {
    let data = serde_json::to_string(&v).unwrap();
    format!(
        "\
            Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\
            Content-Length: {}\r\n\
            \r\n\
            {}\
        ",
        data.len(),
        data
    )
    .into_bytes()
}

struct PendingReader;

impl AsyncRead for PendingReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Pending
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        unreachable!()
    }
}

impl AsyncBufRead for PendingReader {
    fn poll_fill_buf(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        Poll::Pending
    }

    fn consume(self: Pin<&mut Self>, _amt: usize) {
        unreachable!()
    }
}

#[derive(Clone)]
struct RingReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> From<&'a [u8]> for RingReader<'a> {
    fn from(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl AsyncRead for RingReader<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let end = self.pos + buf.len();
        assert!(end <= self.data.len(), "should not read passing messages");
        buf.copy_from_slice(&self.data[self.pos..end]);
        if end == self.data.len() {
            self.pos = 0;
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        unreachable!()
    }
}

impl AsyncBufRead for RingReader<'_> {
    fn poll_fill_buf(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        assert!(self.pos < self.data.len());
        let pos = self.pos;
        Poll::Ready(Ok(&self.get_mut().data[pos..]))
    }

    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        self.pos += amt;
        if self.data.len() == self.pos {
            self.pos = 0;
        }
    }
}

struct CounterWriter {
    limit: u64,
    client: ClientSocket,
}

impl AsyncWrite for CounterWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.starts_with(b"Content-Length: ") {
            self.limit -= 1;
            if self.limit == 0 {
                self.client.emit(()).unwrap();
            }
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
