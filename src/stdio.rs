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
//!
//! [mkfifo(3)]: https://man7.org/linux/man-pages/man3/mkfifo.3.html
//! [pipe(2)]: https://man7.org/linux/man-pages/man2/pipe.2.html
//! [unix(7)]: https://man7.org/linux/man-pages/man7/unix.7.html
//! [tty(4)]: https://man7.org/linux/man-pages/man4/tty.4.html
//! [pty(7)]: https://man7.org/linux/man-pages/man7/pty.7.html
//!
//! When calling [`PipeStdin::lock`], it locks the stdin using [`std::io::stdin`], set its mode to
//! asynchronous, and exposes an a raw [`Read`] interface bypassing the std's buffer.
//!
//! # Caveats
//!
//! Since `PipeStd{in,out}` bypass the std's internal buffer. You should not leave any data in that
//! buffer (via [`std::io::stdin`], [`print!`]-like macros and etc.). Otherwise they will be
//! ignored during `PipeStd{in,out}` operations, which is typically a logic error.
//!
//! # Asynchrous I/O drivers
//!
//! For runtime/drivers with uniform interfaces for sync-async conversion like `async_io::Async`,
//! wrapping `PipeStd{in,out}` inside it just works.
//!
//! ```
//! # async fn work() -> std::io::Result<()> {
//! use futures::AsyncWriteExt;
//!
//! let mut stdout = async_io::Async::new(async_lsp::stdio::PipeStdout::lock()?)?;
//! stdout.write_all(b"truely async, without spawning blocking task!").await?;
//! # Ok(())
//! # }
//! ```
//!
//! For [`tokio`], their interface is
//! [less ergonomic](https://github.com/tokio-rs/tokio/issues/5785).
//! We provide a wrapper method [`PipeStdin::lock_tokio`] to provide an [`tokio::io::AsyncRead`]
//! compatible interface. [`PipeStdout`] works in class.
use std::io::{self, Error, ErrorKind, IoSlice, Read, Result, StdinLock, StdoutLock, Write};
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, RawFd};

use rustix::fs::{fcntl_getfl, fcntl_setfl, fstat, FileType, OFlags};

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
            FileType::Fifo | FileType::Socket | FileType::CharacterDevice
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
    inner: NonBlocking<StdinLock<'static>>,
}

impl PipeStdin {
    /// Lock stdin with pipe-like backend and set it to asynchronous mode.
    ///
    /// # Errors
    ///
    /// Fails if the underlying FD is not pipe-like, or error occurs when setting mode.
    /// See [module level documentation](index.html) for more details.
    pub fn lock() -> Result<Self> {
        let inner = NonBlocking::new(io::stdin().lock())?;
        Ok(Self { inner })
    }
}

impl AsFd for PipeStdin {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner.inner.as_fd()
    }
}

impl AsRawFd for PipeStdin {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.inner.as_raw_fd()
    }
}

// NB. Bypass the internal buffer of `StdinLock` here to keep this in sync with the readiness of
// the underlying FD (which is relied by the I/O re/actor).
impl Read for &'_ PipeStdin {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        rustix::io::read(self, buf).map_err(Into::into)
    }

    fn read_vectored(&mut self, bufs: &mut [io::IoSliceMut<'_>]) -> Result<usize> {
        rustix::io::readv(self, bufs).map_err(Into::into)
    }
}

impl Read for PipeStdin {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        <&PipeStdin>::read(&mut &*self, buf)
    }

    fn read_vectored(&mut self, bufs: &mut [io::IoSliceMut<'_>]) -> Result<usize> {
        <&PipeStdin>::read_vectored(&mut &*self, bufs)
    }
}

/// Locked stdout for asynchronous read.
#[derive(Debug)]
pub struct PipeStdout {
    inner: NonBlocking<StdoutLock<'static>>,
}

impl PipeStdout {
    /// Lock stdout with pipe-like backend and set it to asynchronous mode.
    ///
    /// # Errors
    /// Fails if the underlying FD is not pipe-like, or error occurs when setting mode.
    /// See [module level documentation](index.html) for more details.
    pub fn lock() -> Result<Self> {
        let inner = NonBlocking::new(io::stdout().lock())?;
        Ok(Self { inner })
    }
}

impl AsFd for PipeStdout {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner.inner.as_fd()
    }
}

impl AsRawFd for PipeStdout {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.inner.as_raw_fd()
    }
}

// NB. See `Read` impl.
impl Write for &'_ PipeStdout {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        rustix::io::write(self, buf).map_err(Into::into)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> Result<usize> {
        rustix::io::writev(self, bufs).map_err(Into::into)
    }
}

impl Write for PipeStdout {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        <&PipeStdout>::write(&mut &*self, buf)
    }

    fn flush(&mut self) -> Result<()> {
        <&PipeStdout>::flush(&mut &*self)
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> Result<usize> {
        <&PipeStdout>::write_vectored(&mut &*self, bufs)
    }
}

// Tokio compatibility.
// We can simplify these if we have https://github.com/tokio-rs/tokio/issues/5785
mod tokio_impl {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use futures::ready;
    use tokio::io::unix::AsyncFd;
    use tokio::io::{AsyncRead, AsyncWrite, Interest, ReadBuf};

    use super::*;

    struct TokioPipeStdin {
        inner: AsyncFd<PipeStdin>,
    }

    impl AsyncRead for TokioPipeStdin {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            loop {
                let mut guard = ready!(self.inner.poll_read_ready(cx))?;
                let unfilled = buf.initialize_unfilled();
                match guard.try_io(|inner| inner.get_ref().read(unfilled)) {
                    Ok(Ok(len)) => {
                        buf.advance(len);
                        return Poll::Ready(Ok(()));
                    }
                    Ok(Err(err)) => return Poll::Ready(Err(err)),
                    Err(_would_block) => continue,
                }
            }
        }
    }

    impl PipeStdin {
        /// Shortcut to [`PipeStdin::lock`] and then [`PipeStdin::try_into_tokio`].
        pub fn lock_tokio() -> Result<impl AsyncRead> {
            Self::lock()?.try_into_tokio()
        }

        /// Register the FD to the tokio runtime and return a tokio compatible reader.
        pub fn try_into_tokio(self) -> Result<impl AsyncRead> {
            let inner = AsyncFd::with_interest(self, Interest::READABLE)?;
            Ok(TokioPipeStdin { inner })
        }
    }

    struct TokioPipeStdout {
        inner: AsyncFd<PipeStdout>,
    }

    impl AsyncWrite for TokioPipeStdout {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize>> {
            loop {
                let mut guard = ready!(self.inner.poll_write_ready(cx))?;
                match guard.try_io(|inner| inner.get_ref().write(buf)) {
                    Ok(result) => return Poll::Ready(result),
                    Err(_would_block) => continue,
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl PipeStdout {
        /// Shortcut to [`PipeStdout::lock`] and then [`PipeStdout::try_into_tokio`].
        pub fn lock_tokio() -> Result<impl AsyncWrite> {
            Self::lock()?.try_into_tokio()
        }

        /// Register the FD to the tokio runtime and return a tokio compatible writer.
        pub fn try_into_tokio(self) -> Result<impl AsyncWrite> {
            let inner = AsyncFd::with_interest(self, Interest::WRITABLE)?;
            Ok(TokioPipeStdout { inner })
        }
    }
}
