use std::fs::File;
use std::io::{self, Error, ErrorKind, IoSlice, Result, StdinLock, StdoutLock};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::prelude::FileTypeExt;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::unix::pipe;

#[derive(Debug)]
struct NonBlocking<T: AsRawFd> {
    inner: T,
    prev_flags: libc::c_int,
}

impl<T: AsRawFd> NonBlocking<T> {
    unsafe fn new(inner: T) -> Result<Self> {
        let fd = inner.as_raw_fd();
        let ft = ManuallyDrop::new(File::from_raw_fd(fd))
            .metadata()?
            .file_type();
        if !(ft.is_fifo() || ft.is_socket() || ft.is_char_device()) {
            return Err(Error::new(
                ErrorKind::Other,
                format!("File with mode {ft:?} is not pipe-like"),
            ));
        }

        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || libc::fcntl(inner.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) != 0
        {
            return Err(Error::last_os_error());
        }
        Ok(Self {
            inner,
            prev_flags: flags,
        })
    }
}

impl<T: AsRawFd> Drop for NonBlocking<T> {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::fcntl(self.inner.as_raw_fd(), libc::F_SETFL, self.prev_flags);
        }
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
        let lock = unsafe { NonBlocking::new(io::stdin().lock())? };
        // We must not close the stdin, but we have to pass an owned `File` into tokio.
        // Here we dup the file descripTOR to make it sound.
        // Note that the duplicate shares the same file descriptION.
        let file =
            unsafe { ManuallyDrop::new(File::from_raw_fd(lock.inner.as_raw_fd())).try_clone()? };
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
        let lock = unsafe { NonBlocking::new(io::stdout().lock())? };
        // See `PipeStdin::lock`.
        let file =
            unsafe { ManuallyDrop::new(File::from_raw_fd(lock.inner.as_raw_fd())).try_clone()? };
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
