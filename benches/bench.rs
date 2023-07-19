use std::io;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use async_lsp::{AnyEvent, AnyNotification, AnyRequest, LspService, MainLoop, ResponseError};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use futures::future::Pending;
use futures::{AsyncBufRead, AsyncRead, AsyncWrite};
use lsp_types::notification::{LogMessage, Notification};
use lsp_types::{LogMessageParams, MessageType};
use serde_json::json;
use tower_service::Service;

criterion_group!(benches, bench);
criterion_main!(benches);

fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    c.bench_function("input-notification", |b| {
        let frame = gen_lsp_frame(json!({
            "jsonrpc": "2.0",
            "method": LogMessage::METHOD.to_owned(),
            "params": serde_json::to_value(LogMessageParams {
                typ: MessageType::LOG,
                message: "log".into(),
            }).unwrap(),
        }));
        b.to_async(&rt).iter_custom(|iters| {
            let input = RingReader::from(&*frame);
            async move {
                let (mainloop, _client) = MainLoop::new_server(|_| TestService {
                    count_notifications: iters,
                });
                let fut = mainloop.run(black_box(input), futures::io::sink());
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
            let output = Pin::new(&mut output) as Pin<&mut dyn AsyncWrite>;
            let fut = black_box(mainloop).run(PendingReader, black_box(output));
            let inst = Instant::now();
            fut.await.unwrap();
            inst.elapsed()
        });
    });
}

struct TestService {
    count_notifications: u64,
}

impl Service<AnyRequest> for TestService {
    type Response = serde_json::Value;
    type Error = ResponseError;
    type Future = Pending<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: AnyRequest) -> Self::Future {
        unreachable!()
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

fn gen_lsp_frame(v: serde_json::Value) -> Vec<u8> {
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

#[derive(Clone)]
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
        let len = buf.len().min(self.data.len() - self.pos);
        assert_ne!(len, 0);
        buf[..len].copy_from_slice(&self.data[self.pos..][..len]);
        self.pos += len;
        if self.data.len() == self.pos {
            self.pos = 0;
        }
        Poll::Ready(Ok(len))
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
