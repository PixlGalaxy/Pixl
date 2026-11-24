//! Limits of the admin listener: a fixed-window rate limiter (sign-ins) and a TCP listener that
//! accepts at most N connections at a time.

use std::{
    hash::Hash,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use dashmap::DashMap;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
};

/// At most `limit` hits per `window` per key; bounded to `max_keys` keys.
pub struct RateLimiter<K> {
    map: DashMap<K, (Instant, u32)>,
    limit: u32,
    window: Duration,
    max_keys: usize,
}

impl<K: Eq + Hash + Clone> RateLimiter<K> {
    pub fn new(limit: u32, window: Duration, max_keys: usize) -> Self {
        Self {
            map: DashMap::new(),
            limit,
            window,
            max_keys: max_keys.max(1),
        }
    }

    /// Count a hit; false when the key is over its limit.
    pub fn allow(&self, key: K) -> bool {
        if self.map.len() >= self.max_keys && !self.map.contains_key(&key) {
            self.sweep();
            if self.map.len() >= self.max_keys {
                self.map.clear();
            }
        }
        let mut e = self.map.entry(key).or_insert((Instant::now(), 0));
        if e.0.elapsed() >= self.window {
            *e = (Instant::now(), 0);
        }
        e.1 += 1;
        e.1 <= self.limit
    }

    pub fn sweep(&self) {
        let w = self.window;
        self.map.retain(|_, (at, _)| at.elapsed() < w);
    }
}

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
