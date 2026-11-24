//! Connection limit of the admin listener: a TCP listener that accepts at most N connections at a
//! time. (Sign-ins use the bot's shared `api::session::RateLimiter`.)

use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
};

/// A TCP listener that waits for a free slot before accepting: at most `max` open connections.
pub struct LimitedListener {
    inner: TcpListener,
    slots: Arc<Semaphore>,
}

impl LimitedListener {
    pub fn new(inner: TcpListener, max: usize) -> Self {
        Self {
            inner,
            slots: Arc::new(Semaphore::new(max.max(1))),
        }
    }
}

/// A connection holding its slot until it closes.
pub struct LimitedIo {
    stream: TcpStream,
    _slot: OwnedSemaphorePermit,
}

impl axum::serve::Listener for LimitedListener {
    type Io = LimitedIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok(slot) = self.slots.clone().acquire_owned().await else {
                // Never closed; just in case, do not spin.
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            };
            match self.inner.accept().await {
                Ok((stream, addr)) => {
                    let _ = stream.set_nodelay(true);
                    return (LimitedIo { stream, _slot: slot }, addr);
                }
                // Out of file descriptors and the like: back off instead of spinning.
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

impl AsyncRead for LimitedIo {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for LimitedIo {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// The peer address of a connection accepted by [`LimitedListener`] (axum `ConnectInfo`).
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, LimitedListener>> for PeerAddr {
    fn connect_info(stream: axum::serve::IncomingStream<'_, LimitedListener>) -> Self {
        Self(*stream.remote_addr())
    }
}
