use std::fs::File;
use std::io::{self, Result};
use std::mem::{self, ManuallyDrop};
use std::os::fd::{AsRawFd, FromRawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::unix::pipe;

#[derive(Debug)]
pub struct PipeStdin {
    inner: pipe::Receiver,
}

impl PipeStdin {
    pub fn lock_forever() -> Result<Self> {
        let fd = io::stdin().as_raw_fd();
        mem::forget(io::stdin().lock());
        // We must not close the stdin, but we have to pass an owned `File` into tokio.
        // Here we dup the file descripTOR to make it sound.
        // Note that the duplicate shares the same file descriptION.
        let file = unsafe { ManuallyDrop::new(File::from_raw_fd(fd)).try_clone()? };
        let inner = pipe::Receiver::from_file(file)?;
        Ok(PipeStdin { inner })
    }
}

impl AsyncRead for PipeStdin {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[derive(Debug)]
pub struct PipeStdout {
    inner: pipe::Sender,
}

impl PipeStdout {
    pub fn lock_forever() -> Result<Self> {
        let fd = io::stdout().as_raw_fd();
        mem::forget(io::stdout().lock());
        // See `PipeStdin::lock_forever`.
        let file = unsafe { ManuallyDrop::new(File::from_raw_fd(fd)).try_clone()? };
        let inner = pipe::Sender::from_file(file)?;
        Ok(PipeStdout { inner })
    }
}

impl AsyncWrite for PipeStdout {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
