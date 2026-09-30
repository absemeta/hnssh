//! Bounds socket write episodes even when Russh is outside its read select.
//!
//! SSH window exhaustion is handled by the frame writer, but a peer can offer
//! ample SSH credit and then stop reading TCP. Russh flushes encrypted packets
//! with write_all + flush outside its input select. This adapter gives such a
//! blocked flush an absolute budget, so its transport task returns and the
//! connection owner cancels/joins browsing work without needing an SSH callback.
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::{Sleep, sleep};

const WRITE_STALL_BUDGET: Duration = Duration::from_secs(2);

pub(crate) struct SshTransport {
    socket: TcpStream,
    stalled_flush: Option<Pin<Box<Sleep>>>,
}

impl SshTransport {
    pub(crate) fn new(socket: TcpStream) -> Self {
        Self {
            socket,
            stalled_flush: None,
        }
    }

    fn poll_budget(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if let Some(deadline) = &mut self.stalled_flush
            && deadline.as_mut().poll(cx).is_ready()
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH transport write stalled",
            ));
        }
        Ok(())
    }

    fn arm_budget(&mut self, cx: &mut Context<'_>) {
        // Do not renew on partial progress: trickle reads cannot indefinitely
        // hold write_all outside Russh's input dispatch. Accepting the entire
        // supplied remainder (or completing flush) ends this episode; Russh's
        // identification write does not call flush. Idle and SSH-window-blocked
        // browsing sessions have no socket write in progress and no deadline.
        let deadline = self
            .stalled_flush
            .get_or_insert_with(|| Box::pin(sleep(WRITE_STALL_BUDGET)));
        let _ = deadline.as_mut().poll(cx);
    }
}

impl AsyncRead for SshTransport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_read(cx, bytes)
    }
}

impl AsyncWrite for SshTransport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.poll_budget(cx)?;
        let result = Pin::new(&mut this.socket).poll_write(cx, bytes);
        if result.is_pending() {
            this.arm_budget(cx);
        } else if matches!(result, Poll::Ready(Ok(written)) if written == bytes.len()) {
            this.stalled_flush = None;
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.poll_budget(cx)?;
        let result = Pin::new(&mut this.socket).poll_flush(cx);
        if result.is_pending() {
            this.arm_budget(cx);
        } else if matches!(result, Poll::Ready(Ok(()))) {
            this.stalled_flush = None;
        }
        result
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.poll_budget(cx)?;
        let result = Pin::new(&mut this.socket).poll_shutdown(cx);
        if result.is_pending() {
            this.arm_budget(cx);
        }
        result
    }
}
