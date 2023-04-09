use std::fs::File;
use std::io::{self, Error, ErrorKind, IoSlice, Result, StdinLock, StdoutLock};
use std::os::unix::io::{AsFd, BorrowedFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use rustix::fs::{fcntl_getfl, fcntl_setfl, fstat, FileType, OFlags};
use rustix::io::{dup, stdin, stdout};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::unix::pipe;

#[derive(Debug)]
struct NonBlocking<T: AsFd> {
    inner: T,
    prev_flags: OFlags,
}

impl<T: AsFd> NonBlocking<T> {
    fn new(inner: T) -> Result<Self> {
        let ft = FileType::from_raw_mode(fstat(&inner)?.st_mode);
        if !matches!(
            ft,
            FileType::Fifo | FileType::Socket | FileType::CharacterDevice | FileType::BlockDevice
        ) {
            return Err(Error::new(
                ErrorKind::Other,
                format!("File with mode {ft:?} is not pipe-like"),
            ));
        }

        let prev_flags = fcntl_getfl(&inner)?;
        fcntl_setfl(&inner, prev_flags | OFlags::NONBLOCK)?;
        Ok(Self { inner, prev_flags })
    }
}

impl<T: AsFd> Drop for NonBlocking<T> {
    fn drop(&mut self) {
        let _: std::result::Result<_, _> = fcntl_setfl(&self.inner, self.prev_flags);
    }
}

impl<T: AsFd> AsFd for NonBlocking<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner.as_fd()
    }
}

#[derive(Debug)]
pub struct PipeStdin {
    inner: pipe::Receiver,
    // NB. `_lock` is dropped after `inner` closes the duplicate.
    // So that we can safely reset the flags.
    _lock: NonBlocking<StdinLock<'static>>,
}

impl PipeStdin {
    pub fn lock() -> Result<Self> {
        let lock = NonBlocking::new(io::stdin().lock())?;
        // We must not close the stdin, but we have to pass an owned `File` into tokio.
        // Here we dup the file descripTOR to make it sound.
        // Note that the duplicate shares the same file descriptION.
        let file = File::from(dup(stdin())?);
        let inner = pipe::Receiver::from_file_unchecked(file)?;
        Ok(Self { inner, _lock: lock })
    }
}

impl AsyncRead for PipeStdin {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[derive(Debug)]
pub struct PipeStdout {
    inner: pipe::Sender,
    // NB. `_lock` is dropped after `inner` closes the duplicate.
    // See `PipeStdin`.
    _lock: NonBlocking<StdoutLock<'static>>,
}

impl PipeStdout {
    pub fn lock() -> Result<Self> {
        let lock = NonBlocking::new(io::stdout().lock())?;
        // See `PipeStdin::lock`.
        let file = File::from(dup(stdout())?);
        let inner = pipe::Sender::from_file_unchecked(file)?;
        Ok(Self { inner, _lock: lock })
    }
}

impl AsyncWrite for PipeStdout {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}
