//! Utilities to deal with stdin/stdout communication channel for Language Servers.
//!
//! Typically Language Servers serves on stdin/stdout by default. But generally they cannot be read
//! or written asynchronously usually, due to technical reasons.
//! (Eg. [`tokio::io::stdin`](https://docs.rs/tokio/1.27.0/tokio/io/fn.stdin.html) delegates reads
//! to blocking threads.)
//!
//! This mod defines [`PipeStdin`] and [`PipeStdout`] for only stdin/stdout with pipe-like
//! backends, which actually supports asynchronous reads and writes. This currently means one of:
//! - FIFO pipes. Eg. named pipes [mkfifo(3)] and unnamed pipes [pipe(2)].
//! - Sockets. Eg. TCP connections and UNIX domain sockets [unix(7)].
//! - Character devices. Eg. [tty(4)] or [pty(7)].
//! - Block devices.
//!
//! [mkfifo(3)]: https://man7.org/linux/man-pages/man3/mkfifo.3.html
//! [pipe(2)]: https://man7.org/linux/man-pages/man2/pipe.2.html
//! [unix(7)]: https://man7.org/linux/man-pages/man7/unix.7.html
//! [tty(4)]: https://man7.org/linux/man-pages/man4/tty.4.html
//! [pty(7)]: https://man7.org/linux/man-pages/man7/pty.7.html
//!
//! When calling [`PipeStdin::lock`], it locks the stdin using [`std::io::stdin`], set its mode to
//! asynchronous, and exposes an `AsyncRead` interface for asynchronous reads. It keeps the lock
//! guard internally, and reset the mode back when dropped, so that you can use [`println!`] as
//! normal after that. [`PipeStdout::lock`] works in class.
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
                format!("File type {ft:?} is not pipe-like"),
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

/// Locked stdin for asynchronous read.
#[derive(Debug)]
pub struct PipeStdin {
    inner: pipe::Receiver,
    // NB. `_lock` is dropped after `inner` closes the duplicate.
    // So that we can safely reset the flags.
    _lock: NonBlocking<StdinLock<'static>>,
}

impl PipeStdin {
    /// Lock stdin with pipe-like backend and set it to asynchronous mode.
    ///
    /// # Errors
    /// Fails if the underlying FD is not pipe-like, or error occurs when setting mode.
    /// See [module level documentation](index.html) for more details.
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

impl AsFd for PipeStdin {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner.as_fd()
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

/// Locked stdout for asynchronous read.
#[derive(Debug)]
pub struct PipeStdout {
    inner: pipe::Sender,
    // NB. `_lock` is dropped after `inner` closes the duplicate.
    // See `PipeStdin`.
    _lock: NonBlocking<StdoutLock<'static>>,
}

impl PipeStdout {
    /// Lock stdout with pipe-like backend and set it to asynchronous mode.
    ///
    /// # Errors
    /// Fails if the underlying FD is not pipe-like, or error occurs when setting mode.
    /// See [module level documentation](index.html) for more details.
    pub fn lock() -> Result<Self> {
        let lock = NonBlocking::new(io::stdout().lock())?;
        // See `PipeStdin::lock`.
        let file = File::from(dup(stdout())?);
        let inner = pipe::Sender::from_file_unchecked(file)?;
        Ok(Self { inner, _lock: lock })
    }
}

impl AsFd for PipeStdout {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner.as_fd()
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
