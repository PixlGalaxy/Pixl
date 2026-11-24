//! Seekable HTTP input for songbird.
//!
//! songbird's own `HttpRequest` reports its stream as not seekable, so symphonia's WebM demuxer can
//! only move forward by reading (and downloading) every byte up to the target, and a backward seek
//! fails with "stream can only be seeked forward". When the byte length is known (googlevideo
//! `clen`), [`SeekableHttpRequest`] instead answers every seek with a new bounded range request
//! (`bytes=target-(len-1)`; YouTube does not reliably answer open-ended ranges).
//!
//! The HTTP source ([`SeekableHttpSource`]) is an async songbird [`AsyncMediaSource`]. symphonia
//! reads synchronously, so [`ReadAhead`] bridges the two with a bounded read-ahead buffer (256 KiB,
//! about 13 s of 160 kbps Opus, so a track never downloads much faster than it plays).
//! songbird's adapter is not used because it is only correct for forward-only sources: after a seek
//! it still delivers the bytes it had read before the seek, and once it hit the end of the stream
//! it never reads again.
//!
//! Nothing here may block forever: songbird decodes every call of the process on a few shared
//! mixer threads (one per 16 calls), and `ReadAhead::read` runs on them. A read that never returns
//! (a range request or body that stalls without closing) freezes every channel on that thread:
//! no audio, no seek, no new track. So range requests have an answer timeout, a body that sends
//! nothing for a while is treated as broken (and reopened once at the current byte), and the
//! synchronous side waits a bounded time for the async side before failing the read or seek.
//!
//! A prefetched track may come with its first bytes on disk ([`HeadFile`], see `heads`): the file
//! is read into memory when the input is created (heads are at most 512 KiB), so probing and the
//! first seconds need no network, and the rest comes from one range request starting where the
//! head ends, opened only when a read gets there.
//!
//! The demuxer's probe seeks back and forth within the first few hundred bytes (seen live: 0, 116,
//! 175, 259, 48). Each seek used to cost a range request (~100 ms), so the source keeps the bytes
//! it already has (the head or the first 256 KiB, and the last 512 KiB the body sent) and serves
//! seeks into them from memory; a seek a short way ahead skips bytes on the open body. Only a seek
//! elsewhere opens a new range request.

use std::{
    collections::VecDeque,
    io::{self, Read, Seek, SeekFrom},
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};

use bytes::{Buf, Bytes};
use futures_util::{
    future::BoxFuture,
    stream::{BoxStream, StreamExt},
    FutureExt,
};
use reqwest::{
    header::{HeaderMap, CONTENT_RANGE, RANGE},
    Client, StatusCode,
};
use songbird::input::{AsyncMediaSource, AudioStream, AudioStreamError, Compose, Input};
use symphonia::core::io::MediaSource;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, ReadBuf},
    runtime::Handle,
    sync::{mpsc, oneshot},
};
use tracing::{debug, warn};

/// Read-ahead between the HTTP body and symphonia: `READ_AHEAD_CHUNKS` chunks of `CHUNK` bytes.
const CHUNK: usize = 16 * 1024;
const READ_AHEAD_CHUNKS: usize = 16;
/// The first bytes of the stream kept in memory when there is no head (seeks into them are free).
const PREFIX_CACHE: usize = 256 * 1024;
/// The last bytes the body sent, kept in memory: more than the read-ahead holds, so a seek back
/// into anything the read-ahead or the demuxer had is free.
const WINDOW_CACHE: usize = 512 * 1024;
/// A seek at most this far ahead of the open body reads and drops the bytes instead of reopening.
const FORWARD_SKIP: u64 = 64 * 1024;

/// How long each step may take before it counts as failed.
#[derive(Clone, Copy, Debug)]
struct Timeouts {
    /// Until a range request is answered (status and headers).
    open: Duration,
    /// Without a single body byte while reading: the body is dropped and reopened once.
    stall: Duration,
    /// The synchronous side waiting for the async side (one read or one seek). Longer than a
    /// stall plus a reopen, so a stall that the source recovers from never fails a read.
    wait: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            open: Duration::from_secs(6),
            stall: Duration::from_secs(4),
            wait: Duration::from_secs(12),
        }
    }
}

/// Where and how to fetch the stream; every request is a bounded range request.
#[derive(Clone, Debug)]
struct RangeRequest {
    client: Client,
    url: String,
    headers: HeaderMap,
    len: u64,
    timeouts: Timeouts,
    /// Range requests made for this input (shared by clones), logged with each one.
    opened: Arc<AtomicU32>,
}

pub(super) type Body = BoxStream<'static, io::Result<Bytes>>;

/// The first `len` bytes of a stream, saved on disk (`path`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadFile {
    pub path: PathBuf,
    pub len: u64,
}

/// One bounded range request `bytes=at-last` with the default timeouts (used to download heads).
pub(super) async fn fetch_range(client: Client, url: String, headers: HeaderMap, total: u64, at: u64, last: u64) -> io::Result<Body> {
    RangeRequest {
        client,
        url,
        headers,
        len: total,
        timeouts: Timeouts::default(),
        opened: Arc::default(),
    }
    .open_span(at, last)
    .await
}

impl RangeRequest {
    /// `GET` with `Range: bytes=at-(len-1)`, answered within `timeouts.open`. Only a 206 whose
    /// `Content-Range` starts at `at` is accepted: anything else would put the demuxer at the
    /// wrong byte. The body fails when it sends nothing for `timeouts.stall`.
    async fn open(self, at: u64) -> io::Result<Body> {
        let last = self.len.saturating_sub(1);
        self.open_span(at, last).await
    }

    /// [`Self::open`] for `bytes=at-last`.
    async fn open_span(self, at: u64, last: u64) -> io::Result<Body> {
        let limit = self.timeouts.open;
        let len = self.len;
        let request = self.opened.fetch_add(1, Ordering::Relaxed) + 1;
        let started = std::time::Instant::now();
        let res = match tokio::time::timeout(limit, self.open_now(at, last)).await {
            Ok(res) => res,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("range request from byte {at} got no answer in {limit:?}"),
            )),
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &res {
            Ok(_) => debug!(from = at, len, elapsed_ms, request, "stream range request answered"),
            Err(e) => warn!(from = at, len, elapsed_ms, request, error = %e, "stream range request failed"),
        }
        res
    }

    async fn open_now(self, at: u64, last: u64) -> io::Result<Body> {
        let resp = self
            .client
            .get(&self.url)
            .headers(self.headers)
            .header(RANGE, format!("bytes={at}-{last}"))
            .send()
            .await
            .map_err(io::Error::other)?;
        let status = resp.status();
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(io::Error::other(format!(
                "range request from byte {at} answered {status}"
            )));
        }
        let start = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(content_range_start);
        if start != Some(at) {
            return Err(io::Error::other(format!(
                "range request from byte {at} answered the range starting at {start:?}"
            )));
        }
        Ok(stall_guarded(resp.bytes_stream().boxed(), self.timeouts.stall))
    }
}

/// `body` as io results, failing with `TimedOut` when no chunk comes for `stall` while it is read
/// (the timer only runs while someone waits for the next chunk, not while the read-ahead is full).
fn stall_guarded(body: BoxStream<'static, reqwest::Result<Bytes>>, stall: Duration) -> Body {
    futures_util::stream::unfold(Some(body), move |state| async move {
        let mut body = state?;
        match tokio::time::timeout(stall, body.next()).await {
            Ok(Some(Ok(bytes))) => Some((Ok(bytes), Some(body))),
            Ok(Some(Err(e))) => Some((Err(io::Error::other(e)), None)),
            Ok(None) => None,
            Err(_) => Some((
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("the stream sent nothing for {stall:?}"),
                )),
                None,
            )),
        }
    })
    .boxed()
}

/// First byte of a `Content-Range: bytes a-b/len` value.
fn content_range_start(v: &str) -> Option<u64> {
    let range = v.trim().strip_prefix("bytes")?.trim_start();
    range.split('-').next()?.trim().parse().ok()
}

/// The parts that are not `Sync` (boxed body stream and request future), behind a mutex that is
/// only ever accessed through `get_mut` (songbird requires `Sync` sources).
#[derive(Default)]
struct Pending {
    /// The open body; its next byte is `SeekableHttpSource::net_at`.
    body: Option<Body>,
    /// A range request in flight, with the byte it starts at (always `net_at`).
    opening: Option<(u64, BoxFuture<'static, io::Result<Body>>)>,
}

/// Async, seekable HTTP stream of a known length. Reads advance `pos`.
///
/// Bytes already at hand are served from memory and never requested again: the `prefix` (the
/// head file, or the first [`PREFIX_CACHE`] bytes the network sent) and the `window` (the last
/// [`WINDOW_CACHE`] bytes the body sent, ending at `net_at`, which covers everything the
/// read-ahead and the demuxer may still want). A read past them continues the open body, skipping
/// up to [`FORWARD_SKIP`] bytes on it; only a read or seek elsewhere opens a new range request.
/// A broken body is retried once from `net_at`.
pub struct SeekableHttpSource {
    req: RangeRequest,
    pos: u64,
    /// Bytes `0..prefix.len()` of the stream.
    prefix: Vec<u8>,
    /// Body chunks covering `net_at - window_len .. net_at`.
    window: VecDeque<Bytes>,
    window_len: u64,
    /// The byte the open (or opening) body delivers next.
    net_at: u64,
    /// A seek in progress: the target, and whether it waits for its own range request.
    seek_to: Option<(u64, bool)>,
    /// The body broke and was reopened; reset by the next successful chunk.
    retried: bool,
    pending: Mutex<Pending>,
}

impl SeekableHttpSource {
    /// At `pos`, with `body` (if any) delivering from `pos`.
    fn new(req: RangeRequest, pos: u64, body: Option<Body>) -> Self {
        Self {
            req,
            pos,
            prefix: Vec::new(),
            window: VecDeque::new(),
            window_len: 0,
            net_at: pos,
            seek_to: None,
            retried: false,
            pending: Mutex::new(Pending { body, opening: None }),
        }
    }

    /// At the start of the stream, serving `head` (its first bytes) from memory: no request until
    /// a read goes past it.
    fn with_head(req: RangeRequest, mut head: Vec<u8>) -> Self {
        head.truncate(req.len.min(head.len() as u64) as usize);
        let at = head.len() as u64;
        let mut src = Self::new(req, 0, None);
        src.prefix = head;
        src.net_at = at;
        src
    }

    fn len(&self) -> u64 {
        self.req.len
    }

    fn pending(&mut self) -> &mut Pending {
        self.pending.get_mut().unwrap_or_else(|p| p.into_inner())
    }

    /// The bytes at hand from `pos` on (prefix or window), if `pos` is in them.
    fn cached(&self, pos: u64) -> Option<&[u8]> {
        if pos < self.prefix.len() as u64 {
            return Some(&self.prefix[pos as usize..]);
        }
        let mut at = self.net_at - self.window_len;
        if pos < at || pos >= self.net_at {
            return None;
        }
        for c in &self.window {
            let end = at + c.len() as u64;
            if pos < end {
                return Some(&c[(pos - at) as usize..]);
            }
            at = end;
        }
        None
    }

    /// The open (or opening) body gets to `pos` by reading at most [`FORWARD_SKIP`] bytes.
    fn net_reaches(&mut self, pos: u64) -> bool {
        let net_at = self.net_at;
        let p = self.pending();
        let open = p.body.is_some() || matches!(&p.opening, Some((at, _)) if *at == net_at);
        open && pos >= net_at && pos - net_at <= FORWARD_SKIP
    }

    /// Keep a body chunk (it starts at `net_at`).
    fn push(&mut self, mut bytes: Bytes) {
        // Never hand out more than the announced length.
        let room = self.len().saturating_sub(self.net_at);
        bytes.truncate(room.min(bytes.len() as u64) as usize);
        if bytes.is_empty() {
            return;
        }
        if self.net_at == self.prefix.len() as u64 && self.prefix.len() < PREFIX_CACHE {
            let take = (PREFIX_CACHE - self.prefix.len()).min(bytes.len());
            self.prefix.extend_from_slice(&bytes[..take]);
        }
        self.net_at += bytes.len() as u64;
        self.window_len += bytes.len() as u64;
        self.window.push_back(bytes);
        while self.window_len > WINDOW_CACHE as u64 && self.window.len() > 1 {
            if let Some(front) = self.window.pop_front() {
                self.window_len -= front.len() as u64;
            }
        }
    }

    /// Make sure a range request from `at` is in flight (dropping the current body). The window
    /// is kept when the request continues where it ends.
    fn ensure_opening(&mut self, at: u64) {
        let req = self.req.clone();
        let pending = self.pending();
        if matches!(&pending.opening, Some((from, _)) if *from == at) {
            return;
        }
        pending.body = None;
        pending.opening = Some((at, req.open(at).boxed()));
        if at != self.net_at {
            self.window.clear();
            self.window_len = 0;
            self.net_at = at;
        }
    }

    /// Poll the request started by `ensure_opening`; on success it becomes the body.
    fn poll_opening(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let pending = self.pending();
        let Some((_, fut)) = &mut pending.opening else {
            return Poll::Ready(Ok(()));
        };
        let res = match fut.poll_unpin(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(r) => r,
        };
        pending.opening = None;
        Poll::Ready(res.map(|body| pending.body = Some(body)))
    }
}

impl AsyncRead for SeekableHttpSource {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            let pos = this.pos;
            if pos >= this.len() || buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            if let Some(at_hand) = this.cached(pos) {
                let n = at_hand.len().min(buf.remaining());
                buf.put_slice(&at_hand[..n]);
                this.pos = pos + n as u64;
                return Poll::Ready(Ok(()));
            }
            if !this.net_reaches(pos) {
                this.ensure_opening(pos);
            }
            if let Err(e) = std::task::ready!(this.poll_opening(cx)) {
                return Poll::Ready(Err(e));
            }
            let Some(body) = &mut this.pending().body else { continue };
            let err = match std::task::ready!(body.poll_next_unpin(cx)) {
                Some(Ok(bytes)) => {
                    this.push(bytes);
                    this.retried = false;
                    continue;
                }
                Some(Err(e)) => e,
                None => io::Error::new(io::ErrorKind::UnexpectedEof, "the stream ended early"),
            };
            this.pending().body = None;
            if this.retried {
                warn!(error = %err, pos, "stream read failed again after reopening it");
                return Poll::Ready(Err(err));
            }
            let at = this.net_at;
            warn!(error = %err, pos, at, "stream read failed; reopening it where it broke");
            this.retried = true;
            this.ensure_opening(at);
        }
    }
}

impl AsyncSeek for SeekableHttpSource {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        let this = self.get_mut();
        let target = match position {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => this.pos.checked_add_signed(d),
            SeekFrom::End(d) => this.len().checked_add_signed(d),
        };
        let Some(target) = target else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"));
        };
        this.seek_to = Some((target, false));
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        let Some((target, requested)) = this.seek_to else {
            return Poll::Ready(Ok(this.pos));
        };
        // At the end, in the bytes at hand, or a short way ahead on the open body: no request.
        if !requested && (target >= this.len() || this.cached(target).is_some() || this.net_reaches(target)) {
            this.pos = target;
            this.seek_to = None;
            return Poll::Ready(Ok(target));
        }
        // Elsewhere: the seek waits for its range request, so a refused one fails the seek.
        this.seek_to = Some((target, true));
        this.ensure_opening(target);
        let res = std::task::ready!(this.poll_opening(cx));
        this.seek_to = None;
        // On failure the next read reopens at `pos`.
        Poll::Ready(res.map(|()| {
            this.pos = target;
            this.retried = false;
            target
        }))
    }
}

#[serenity::async_trait]
impl AsyncMediaSource for SeekableHttpSource {
    fn is_seekable(&self) -> bool {
        true
    }

    async fn byte_len(&self) -> Option<u64> {
        Some(self.len())
    }

    async fn try_resume(&mut self, offset: u64) -> Result<Box<dyn AsyncMediaSource>, AudioStreamError> {
        let body = self
            .req
            .clone()
            .open(offset)
            .await
            .map_err(|e| AudioStreamError::Fail(Box::new(e)))?;
        Ok(Box::new(SeekableHttpSource::new(self.req.clone(), offset, Some(body))))
    }
}

enum Msg {
    Data(u64, Bytes),
    Eof(u64),
    Err(u64, io::Error),
}

enum Request {
    /// Seek to `to`; on failure go back to `current` (the reader's position). Data sent afterwards
    /// is tagged with `epoch`, so the reader can drop what was read ahead before the seek.
    Seek {
        to: u64,
        current: u64,
        epoch: u64,
        reply: oneshot::Sender<SeekReply>,
    },
    Len {
        reply: oneshot::Sender<Option<u64>>,
    },
}

struct SeekReply {
    res: io::Result<u64>,
    /// The seek failed and the source could not go back to the reader's position either: nothing
    /// more comes for this epoch.
    lost: bool,
}

/// Synchronous symphonia source over an [`AsyncMediaSource`], with a bounded read-ahead buffer
/// filled by a tokio task. Must be created inside a tokio runtime; reads block the calling thread
/// (songbird's mixer and blocking pool), for at most `wait` each.
pub struct ReadAhead {
    data: Mutex<mpsc::Receiver<Msg>>,
    requests: mpsc::UnboundedSender<Request>,
    rt: Handle,
    wait: Duration,
    seekable: bool,
    epoch: u64,
    pos: u64,
    chunk: Bytes,
    /// End of stream or a read error seen for the current epoch: nothing more comes until a seek.
    stopped: Option<Stopped>,
}

#[derive(Clone, Copy)]
enum Stopped {
    Eof,
    Failed(io::ErrorKind),
}

impl ReadAhead {
    fn with_wait(source: Box<dyn AsyncMediaSource>, wait: Duration) -> Self {
        let seekable = source.is_seekable();
        let (data_tx, data_rx) = mpsc::channel(READ_AHEAD_CHUNKS);
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        tokio::spawn(pump(source, data_tx, req_rx));
        Self {
            data: Mutex::new(data_rx),
            requests: req_tx,
            rt: Handle::current(),
            wait,
            seekable,
            epoch: 0,
            pos: 0,
            chunk: Bytes::new(),
            stopped: None,
        }
    }

    fn gone() -> io::Error {
        io::Error::new(io::ErrorKind::UnexpectedEof, "the stream reader stopped")
    }

    /// Wait for `fut` (driven by the runtime) for at most `wait`.
    fn wait_for<F: std::future::Future>(rt: &Handle, wait: Duration, fut: F) -> io::Result<F::Output> {
        // The timer must be created inside `block_on` (which enters the runtime): songbird calls
        // this from its own non-Tokio threads, where `timeout()` itself would panic.
        rt.block_on(async move { tokio::time::timeout(wait, fut).await }).map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the stream did not answer in {wait:?}"),
            )
        })
    }
}

/// The async half: fills the read-ahead channel and serves seek / length requests.
async fn pump(
    mut src: Box<dyn AsyncMediaSource>,
    tx: mpsc::Sender<Msg>,
    mut requests: mpsc::UnboundedReceiver<Request>,
) {
    let mut buf = vec![0u8; CHUNK];
    let mut epoch = 0;
    let mut stopped = false;
    loop {
        let permit = if stopped {
            None
        } else {
            tokio::select! {
                biased;
                r = requests.recv() => match r {
                    Some(r) => {
                        serve(src.as_mut(), r, &mut epoch, &mut stopped).await;
                        continue;
                    }
                    None => return,
                },
                p = tx.reserve() => match p {
                    Ok(p) => Some(p),
                    Err(_) => return,
                },
            }
        };
        let Some(permit) = permit else {
            match requests.recv().await {
                Some(r) => serve(src.as_mut(), r, &mut epoch, &mut stopped).await,
                None => return,
            }
            continue;
        };
        // A request interrupts a slow read (reads are cancel safe).
        let read = tokio::select! {
            biased;
            r = requests.recv() => Err(r),
            n = src.read(&mut buf) => Ok(n),
        };
        match read {
            Err(Some(r)) => {
                drop(permit);
                serve(src.as_mut(), r, &mut epoch, &mut stopped).await;
            }
            Err(None) => return,
            Ok(Ok(0)) => {
                permit.send(Msg::Eof(epoch));
                stopped = true;
            }
            Ok(Ok(n)) => permit.send(Msg::Data(epoch, Bytes::copy_from_slice(&buf[..n]))),
            Ok(Err(e)) => {
                permit.send(Msg::Err(epoch, e));
                stopped = true;
            }
        }
    }
}

async fn serve(src: &mut dyn AsyncMediaSource, req: Request, epoch: &mut u64, stopped: &mut bool) {
    match req {
        Request::Len { reply } => {
            let _ = reply.send(src.byte_len().await);
        }
        Request::Seek {
            to,
            current,
            epoch: e,
            reply,
        } => {
            *epoch = e;
            *stopped = false;
            let reply_with = match src.seek(SeekFrom::Start(to)).await {
                Ok(p) => SeekReply {
                    res: Ok(p),
                    lost: false,
                },
                Err(e) => {
                    warn!(error = %e, to, current, "stream seek failed");
                    // Keep reading from where the reader is, so playback can go on.
                    let lost = match src.seek(SeekFrom::Start(current)).await {
                        Ok(_) => false,
                        Err(back) => {
                            warn!(error = %back, current, "could not return to the current position");
                            *stopped = true;
                            true
                        }
                    };
                    SeekReply { res: Err(e), lost }
                }
            };
            let _ = reply.send(reply_with);
        }
    }
}

impl Read for ReadAhead {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if !self.chunk.is_empty() {
                let n = self.chunk.len().min(out.len());
                out[..n].copy_from_slice(&self.chunk[..n]);
                self.chunk.advance(n);
                self.pos += n as u64;
                return Ok(n);
            }
            if out.is_empty() {
                return Ok(0);
            }
            match self.stopped {
                Some(Stopped::Eof) => return Ok(0),
                Some(Stopped::Failed(kind)) => return Err(io::Error::new(kind, "the stream failed")),
                None => {}
            }
            let rx = self.data.get_mut().unwrap_or_else(|p| p.into_inner());
            let msg = match Self::wait_for(&self.rt, self.wait, rx.recv()) {
                Ok(Some(msg)) => msg,
                Ok(None) => return Err(Self::gone()),
                Err(e) => {
                    // Never block the mixer thread for good: this input is done.
                    warn!(pos = self.pos, error = %e, "stream read timed out");
                    self.stopped = Some(Stopped::Failed(e.kind()));
                    return Err(e);
                }
            };
            match msg {
                Msg::Data(e, b) if e == self.epoch => self.chunk = b,
                Msg::Eof(e) if e == self.epoch => self.stopped = Some(Stopped::Eof),
                Msg::Err(e, err) if e == self.epoch => {
                    self.stopped = Some(Stopped::Failed(err.kind()));
                    return Err(err);
                }
                // Read ahead before the last seek.
                _ => {}
            }
        }
    }
}

impl Seek for ReadAhead {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        if !self.seekable {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "the stream is not seekable"));
        }
        let target = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.byte_len().and_then(|l| l.checked_add_signed(d)),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid seek target"))?;
        // Inside the chunk we already hold: no request.
        if target >= self.pos && target - self.pos <= self.chunk.len() as u64 {
            self.chunk.advance((target - self.pos) as usize);
            self.pos = target;
            return Ok(target);
        }
        let current = self.pos;
        self.epoch += 1;
        self.chunk.clear();
        self.stopped = None;
        let (reply, rx) = oneshot::channel();
        self.requests
            .send(Request::Seek {
                to: target,
                current,
                epoch: self.epoch,
                reply,
            })
            .map_err(|_| Self::gone())?;
        let reply = match Self::wait_for(&self.rt, self.wait, rx) {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => return Err(Self::gone()),
            Err(e) => {
                // The source may still land anywhere: this input is done.
                warn!(to = target, error = %e, "stream seek timed out");
                self.stopped = Some(Stopped::Failed(e.kind()));
                return Err(e);
            }
        };
        match &reply.res {
            Ok(p) => self.pos = *p,
            Err(e) if reply.lost => self.stopped = Some(Stopped::Failed(e.kind())),
            Err(_) => {}
        }
        reply.res
    }
}

impl MediaSource for ReadAhead {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        let (reply, rx) = oneshot::channel();
        self.requests.send(Request::Len { reply }).ok()?;
        Self::wait_for(&self.rt, self.wait, rx).ok()?.ok().flatten()
    }
}

/// Lazy songbird input for a stream of known byte length, seekable with range requests.
#[derive(Clone, Debug)]
pub struct SeekableHttpRequest {
    req: RangeRequest,
    head: Option<HeadFile>,
}

impl SeekableHttpRequest {
    pub fn new(client: Client, url: String, headers: HeaderMap, len: u64) -> Self {
        Self {
            req: RangeRequest {
                client,
                url,
                headers,
                len,
                timeouts: Timeouts::default(),
                opened: Arc::default(),
            },
            head: None,
        }
    }

    /// Serve the first bytes from `head` (a prefetched head of this exact stream).
    pub fn with_head(mut self, head: Option<HeadFile>) -> Self {
        self.head = head.filter(|h| h.len > 0);
        self
    }

    /// The async source at the start of the stream: on the head when there is one (read from its
    /// file; no request yet), else with the first range request already answered.
    async fn source(&self) -> io::Result<SeekableHttpSource> {
        if let Some(head) = &self.head {
            match tokio::fs::read(&head.path).await {
                Ok(mut data) => {
                    if (data.len() as u64) < head.len {
                        warn!(path = %head.path.display(), len = data.len(), expected = head.len, "the head file is short; the network takes over where it ends");
                    }
                    data.truncate(head.len.min(data.len() as u64) as usize);
                    if !data.is_empty() {
                        return Ok(SeekableHttpSource::with_head(self.req.clone(), data));
                    }
                }
                Err(e) => warn!(path = %head.path.display(), error = %e, "could not read the head file; reading from the network"),
            }
        }
        let body = self.req.clone().open(0).await?;
        Ok(SeekableHttpSource::new(self.req.clone(), 0, Some(body)))
    }
}

#[serenity::async_trait]
impl Compose for SeekableHttpRequest {
    fn create(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        Err(AudioStreamError::Unsupported)
    }

    async fn create_async(&mut self) -> Result<AudioStream<Box<dyn MediaSource>>, AudioStreamError> {
        let source = self.source().await.map_err(|e| AudioStreamError::Fail(Box::new(e)))?;
        Ok(AudioStream {
            input: Box::new(ReadAhead::with_wait(Box::new(source), self.req.timeouts.wait)) as Box<dyn MediaSource>,
        })
    }

    fn should_create_async(&self) -> bool {
        true
    }
}

impl From<SeekableHttpRequest> for Input {
    fn from(val: SeekableHttpRequest) -> Self {
        Input::Lazy(Box::new(val))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex as StdMutex},
    };

    use axum::{
        extract::State,
        http::{HeaderMap as AxumHeaders, StatusCode as AxumStatus},
        response::{IntoResponse, Response},
        routing::get,
        Router,
    };

    use super::*;

    /// Serves `data` with bounded `Range` support (206 + Content-Range) and records every range.
    #[derive(Clone)]
    struct Server {
        data: Arc<Vec<u8>>,
        ranges: Arc<StdMutex<Vec<String>>>,
        /// The next answer sends this many bytes, then nothing more without closing.
        stall_after: Arc<StdMutex<Option<usize>>>,
        /// Answer 403 (an expired or refused URL) once this many requests were served.
        refuse_after: Arc<StdMutex<Option<usize>>>,
    }

    async fn serve_file(State(s): State<Server>, headers: AxumHeaders) -> Response {
        let range = headers
            .get("range")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let served = {
            let mut ranges = s.ranges.lock().unwrap();
            ranges.push(range.clone());
            ranges.len() - 1
        };
        if s.refuse_after.lock().unwrap().is_some_and(|n| served >= n) {
            return AxumStatus::FORBIDDEN.into_response();
        }
        let len = s.data.len() as u64;
        let Some((a, b)) = range.strip_prefix("bytes=").and_then(|r| r.split_once('-')) else {
            return (AxumStatus::OK, s.data.to_vec()).into_response();
        };
        let (Ok(a), Ok(b)) = (a.parse::<u64>(), b.parse::<u64>()) else {
            // Open-ended ranges are not answered, like YouTube.
            return AxumStatus::FORBIDDEN.into_response();
        };
        if a > b || b >= len {
            return AxumStatus::RANGE_NOT_SATISFIABLE.into_response();
        }
        let body = s.data[a as usize..=b as usize].to_vec();
        let content_range = [("content-range", format!("bytes {a}-{b}/{len}"))];
        if let Some(n) = s.stall_after.lock().unwrap().take() {
            let first = Bytes::from(body[..n.min(body.len())].to_vec());
            let stream = futures_util::stream::once(async move { Ok::<_, io::Error>(first) })
                .chain(futures_util::stream::pending());
            return (
                AxumStatus::PARTIAL_CONTENT,
                content_range,
                axum::body::Body::from_stream(stream),
            )
                .into_response();
        }
        (AxumStatus::PARTIAL_CONTENT, content_range, body).into_response()
    }

    async fn start(size: usize) -> (String, Server) {
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let server = Server {
            data: Arc::new(data),
            ranges: Arc::default(),
            stall_after: Arc::default(),
            refuse_after: Arc::default(),
        };
        let app = Router::new().route("/file", get(serve_file)).with_state(server.clone());
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/file"), server)
    }

    fn request(url: &str, len: u64) -> SeekableHttpRequest {
        SeekableHttpRequest::new(Client::new(), url.to_owned(), HeaderMap::new(), len)
    }

    fn ranges(s: &Server) -> Vec<String> {
        s.ranges.lock().unwrap().clone()
    }

    #[test]
    fn parses_content_range() {
        assert_eq!(content_range_start("bytes 100-199/1000"), Some(100));
        assert_eq!(content_range_start("bytes 0-0/1"), Some(0));
        assert_eq!(content_range_start("items 1-2/3"), None);
    }

    #[tokio::test]
    async fn async_source_seeks_with_bounded_ranges() {
        const SIZE: usize = 2_000_000;
        let (url, server) = start(SIZE).await;
        let mut src = request(&url, SIZE as u64).source().await.unwrap();
        assert!(AsyncMediaSource::is_seekable(&src));
        assert_eq!(src.byte_len().await, Some(SIZE as u64));

        let mut first = vec![0u8; 10_000];
        src.read_exact(&mut first).await.unwrap();
        assert_eq!(first, server.data[..10_000]);

        // Forward far away (a request), then back into the first bytes (kept in memory).
        let mut buf = [0u8; 100];
        assert_eq!(src.seek(SeekFrom::Start(1_500_000)).await.unwrap(), 1_500_000);
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[1_500_000..1_500_100]);
        assert_eq!(src.seek(SeekFrom::Start(5_000)).await.unwrap(), 5_000);
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[5_000..5_100]);
        // Relative and end-based seeks.
        assert_eq!(src.seek(SeekFrom::Current(-50)).await.unwrap(), 5_050);
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[5_050..5_150]);
        assert_eq!(src.seek(SeekFrom::End(-10)).await.unwrap(), SIZE as u64 - 10);
        let mut rest = Vec::new();
        src.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, server.data[SIZE - 10..]);
        // Far back, outside everything kept: a request.
        assert_eq!(src.seek(SeekFrom::Start(600_000)).await.unwrap(), 600_000);
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[600_000..600_100]);

        let last = SIZE - 1;
        assert_eq!(
            ranges(&server),
            vec![
                format!("bytes=0-{last}"),
                format!("bytes=1500000-{last}"),
                format!("bytes={}-{last}", SIZE - 10),
                format!("bytes=600000-{last}"),
            ]
        );
        assert!(src.seek(SeekFrom::Current(-(SIZE as i64) - 1)).await.is_err());
    }

    /// A seek a short way ahead of the open body reads and drops the bytes on it (no request);
    /// a far one opens a new range.
    #[tokio::test]
    async fn short_forward_seek_skips_on_the_open_body() {
        const SIZE: usize = 2_000_000;
        let (url, server) = start(SIZE).await;
        let req = request(&url, SIZE as u64).req;
        let body = req.clone().open(0).await.unwrap();
        // Nothing read yet: the body is at byte 0 and nothing is kept.
        let mut src = SeekableHttpSource::new(req, 0, Some(body));
        let mut buf = [0u8; 100];
        assert_eq!(src.seek(SeekFrom::Start(30_000)).await.unwrap(), 30_000);
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[30_000..30_100]);
        assert_eq!(ranges(&server).len(), 1);
        assert_eq!(src.seek(SeekFrom::Start(1_500_000)).await.unwrap(), 1_500_000);
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[1_500_000..1_500_100]);
        assert_eq!(src.seek(SeekFrom::Start(1_550_000)).await.unwrap(), 1_550_000);
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[1_550_000..1_550_100]);
        let last = SIZE - 1;
        assert_eq!(
            ranges(&server),
            vec![format!("bytes=0-{last}"), format!("bytes=1500000-{last}")]
        );
    }

    #[tokio::test]
    async fn async_source_rejects_a_wrong_range_answer() {
        // The server serves 1000 bytes, but we claim 2000: the range request is refused.
        let (url, _server) = start(1_000).await;
        assert!(request(&url, 2_000).source().await.is_err());
        // A seek that fails keeps the current position, and reading goes on from there.
        let (url, server) = start(1_000).await;
        let mut src = SeekableHttpSource::new(
            RangeRequest {
                client: Client::new(),
                url: url.replace("/file", "/missing"),
                headers: HeaderMap::new(),
                len: 1_000,
                timeouts: Timeouts::default(),
                opened: Arc::default(),
            },
            0,
            None,
        );
        assert!(src.seek(SeekFrom::Start(10)).await.is_err());
        src.req.url = url;
        let mut buf = [0u8; 10];
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[..10]);
    }

    fn quick_timeouts() -> Timeouts {
        Timeouts {
            open: Duration::from_millis(300),
            stall: Duration::from_millis(300),
            wait: Duration::from_secs(2),
        }
    }

    /// A body that stops sending without closing (seen live: the whole call went silent) is
    /// dropped after the stall timeout and reopened at the byte the reader is at.
    #[tokio::test]
    async fn stalled_body_is_reopened_at_the_current_byte() {
        const SIZE: usize = 50_000;
        let (url, server) = start(SIZE).await;
        *server.stall_after.lock().unwrap() = Some(1_000);
        let mut req = request(&url, SIZE as u64);
        req.req.timeouts = quick_timeouts();
        let mut src = req.source().await.unwrap();
        let mut all = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), src.read_to_end(&mut all))
            .await
            .expect("the stall is recovered")
            .unwrap();
        assert_eq!(all, *server.data);
        let last = SIZE - 1;
        assert_eq!(
            ranges(&server),
            vec![format!("bytes=0-{last}"), format!("bytes=1000-{last}")]
        );
    }

    /// The body stalls and the reopened range is refused (403): the read fails within the timeouts,
    /// with the status in the error, instead of hanging (the player then resolves the URL again).
    #[tokio::test]
    async fn stall_then_refused_range_fails_the_read() {
        const SIZE: usize = 50_000;
        let (url, server) = start(SIZE).await;
        *server.stall_after.lock().unwrap() = Some(1_000);
        *server.refuse_after.lock().unwrap() = Some(1);
        let mut req = request(&url, SIZE as u64);
        req.req.timeouts = quick_timeouts();
        let mut src = req.source().await.unwrap();
        let mut all = Vec::new();
        let err = tokio::time::timeout(Duration::from_secs(5), src.read_to_end(&mut all))
            .await
            .expect("no hang")
            .unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
        assert_eq!(all, server.data[..1_000]);
    }

    /// A range request that is never answered fails after the open timeout (a seek there fails
    /// instead of hanging, and the source stays where it was).
    #[tokio::test]
    async fn unanswered_range_request_times_out() {
        // Accepts connections but never answers.
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        let mut req = request(&format!("http://{addr}/file"), 1_000);
        req.req.timeouts = quick_timeouts();
        let started = std::time::Instant::now();
        let err = req.source().await.err().expect("no answer");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// An async source that never delivers anything.
    struct Never;

    impl AsyncRead for Never {
        fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncSeek for Never {
        fn start_seek(self: Pin<&mut Self>, _: SeekFrom) -> io::Result<()> {
            Ok(())
        }

        fn poll_complete(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<u64>> {
            Poll::Pending
        }
    }

    #[serenity::async_trait]
    impl AsyncMediaSource for Never {
        fn is_seekable(&self) -> bool {
            true
        }

        async fn byte_len(&self) -> Option<u64> {
            Some(1_000)
        }
    }

    /// The synchronous side (songbird's shared mixer thread) never waits for good: a read or a
    /// seek that gets no answer fails after `wait`, and the input stays failed.
    /// Run `f` on a plain OS thread with no Tokio context, like songbird's mixer and decode
    /// threads (`spawn_blocking` would enter the runtime and hide context bugs).
    async fn on_plain_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, &'static str> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.await.map_err(|_| "the plain thread panicked")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_ahead_waits_a_bounded_time() {
        let mut stream = ReadAhead::with_wait(Box::new(Never), Duration::from_millis(200));
        on_plain_thread(move || {
            let started = std::time::Instant::now();
            let mut buf = [0u8; 10];
            let err = stream.read(&mut buf).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::TimedOut);
            assert!(stream.read(&mut buf).is_err(), "a timed out input stays failed");
            let err = stream.seek(SeekFrom::Start(500)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::TimedOut);
            assert!(started.elapsed() < Duration::from_secs(2));
        })
        .await
        .unwrap();
    }

    /// Sync side, as symphonia uses it: reads, forward and backward seeks with a full read-ahead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_ahead_seeks_forward_and_backward() {
        const SIZE: usize = 1_000_000;
        let (url, server) = start(SIZE).await;
        let req = request(&url, SIZE as u64);
        let AudioStream { input: mut stream, .. } = req.clone().create_async().await.unwrap();
        assert!(stream.is_seekable());
        let data = server.data.clone();
        on_plain_thread(move || {
            assert_eq!(stream.byte_len(), Some(SIZE as u64));
            let mut buf = vec![0u8; 1000];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(buf, data[..1000]);
            // Let the read-ahead fill up, so stale bytes would show up after the seeks.
            std::thread::sleep(std::time::Duration::from_millis(200));
            assert_eq!(stream.seek(SeekFrom::Start(600_000)).unwrap(), 600_000);
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(buf, data[600_000..601_000]);
            std::thread::sleep(std::time::Duration::from_millis(200));
            assert_eq!(stream.seek(SeekFrom::Start(20_000)).unwrap(), 20_000);
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(buf, data[20_000..21_000]);
            // Small skip inside the chunk already held: no new request.
            assert_eq!(stream.seek(SeekFrom::Current(100)).unwrap(), 21_100);
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(buf, data[21_100..22_100]);
            // Read to the end, then seek back: reading starts again.
            assert_eq!(stream.seek(SeekFrom::End(-500)).unwrap(), SIZE as u64 - 500);
            let mut rest = Vec::new();
            stream.read_to_end(&mut rest).unwrap();
            assert_eq!(rest, data[SIZE - 500..]);
            assert_eq!(stream.seek(SeekFrom::Start(0)).unwrap(), 0);
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(buf, data[..1000]);
        })
        .await
        .unwrap();
        // The seeks back to 20 000 and 0 are served from the first bytes kept in memory; reading
        // on past them may continue from where that memory ends.
        let last = SIZE - 1;
        let r = ranges(&server);
        assert_eq!(r[..2], [format!("bytes=0-{last}"), format!("bytes=600000-{last}")], "{r:?}");
        let end = format!("bytes={}-{last}", SIZE - 500);
        let after_prefix = format!("bytes={PREFIX_CACHE}-{last}");
        assert!(r.contains(&end), "{r:?}");
        assert!(r[2..].iter().all(|x| *x == end || *x == after_prefix), "{r:?}");
    }

    /// The read-ahead is bounded: a paused reader does not download the whole file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_ahead_is_bounded() {
        const SIZE: usize = 4_000_000;
        let (url, _server) = start(SIZE).await;
        let src = request(&url, SIZE as u64).source().await.unwrap();
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counting = Counting {
            inner: src,
            read: counter.clone(),
        };
        let stream = ReadAhead::with_wait(Box::new(counting), Timeouts::default().wait);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let read = counter.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            read <= ((READ_AHEAD_CHUNKS + 2) * CHUNK) as u64,
            "read {read} bytes ahead"
        );
        drop(stream);
    }

    struct Counting {
        inner: SeekableHttpSource,
        read: Arc<std::sync::atomic::AtomicU64>,
    }

    impl AsyncRead for Counting {
        fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            let before = buf.filled().len();
            let res = Pin::new(&mut self.inner).poll_read(cx, buf);
            let n = (buf.filled().len() - before) as u64;
            self.read.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            res
        }
    }

    impl AsyncSeek for Counting {
        fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
            Pin::new(&mut self.inner).start_seek(position)
        }

        fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
            Pin::new(&mut self.inner).poll_complete(cx)
        }
    }

    #[serenity::async_trait]
    impl AsyncMediaSource for Counting {
        fn is_seekable(&self) -> bool {
            true
        }

        async fn byte_len(&self) -> Option<u64> {
            Some(self.inner.len())
        }
    }

    /// Write the first `len` bytes of `data` to a temp file and describe it as a head.
    fn head_of(data: &[u8], len: usize) -> HeadFile {
        let path = std::env::temp_dir().join(format!("lynx-head-{}.bin", uuid::Uuid::new_v4()));
        std::fs::write(&path, &data[..len]).unwrap();
        HeadFile { path, len: len as u64 }
    }

    /// The head comes from the file, the rest from one range request that starts where the file
    /// ends: the bytes are exactly the stream's.
    #[tokio::test]
    async fn head_file_then_network_gives_identical_bytes() {
        const SIZE: usize = 300_000;
        let (url, server) = start(SIZE).await;
        let head = head_of(&server.data, 100_000);
        let mut src = request(&url, SIZE as u64).with_head(Some(head.clone())).source().await.unwrap();
        assert!(ranges(&server).is_empty(), "nothing is requested before the head is read");
        let mut first = vec![0u8; 100_000];
        src.read_exact(&mut first).await.unwrap();
        assert_eq!(first, server.data[..100_000]);
        assert!(ranges(&server).is_empty(), "the head is served from disk");
        let mut rest = Vec::new();
        src.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, server.data[100_000..]);
        assert_eq!(ranges(&server), vec![format!("bytes=100000-{}", SIZE - 1)]);
        let _ = std::fs::remove_file(head.path);
    }

    /// Seeks inside the head read the file again (no request); seeks past it are range requests;
    /// a read across the end of the head continues from the network at the right byte.
    #[tokio::test]
    async fn seeks_inside_and_outside_the_head() {
        const SIZE: usize = 300_000;
        let (url, server) = start(SIZE).await;
        let head = head_of(&server.data, 100_000);
        let mut src = request(&url, SIZE as u64).with_head(Some(head.clone())).source().await.unwrap();
        let mut buf = [0u8; 100];
        assert_eq!(src.seek(SeekFrom::Start(50_000)).await.unwrap(), 50_000);
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[50_000..50_100]);
        assert!(ranges(&server).is_empty());
        assert_eq!(src.seek(SeekFrom::Start(200_000)).await.unwrap(), 200_000);
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[200_000..200_100]);
        // Back into the head, then across its end.
        assert_eq!(src.seek(SeekFrom::Start(99_950)).await.unwrap(), 99_950);
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[99_950..100_050]);
        assert_eq!(src.seek(SeekFrom::Start(10)).await.unwrap(), 10);
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[10..110]);
        let last = SIZE - 1;
        assert_eq!(
            ranges(&server),
            vec![format!("bytes=200000-{last}"), format!("bytes=100000-{last}")]
        );
        let _ = std::fs::remove_file(head.path);
    }

    /// A missing head file means a normal network start; a head file shorter than announced is
    /// used as far as it goes and the network takes over at that byte.
    #[tokio::test]
    async fn broken_head_files_fall_back_to_the_network() {
        const SIZE: usize = 200_000;
        let (url, server) = start(SIZE).await;
        let missing = HeadFile {
            path: std::env::temp_dir().join(format!("lynx-head-missing-{}", uuid::Uuid::new_v4())),
            len: 50_000,
        };
        let mut src = request(&url, SIZE as u64).with_head(Some(missing)).source().await.unwrap();
        let mut all = Vec::new();
        src.read_to_end(&mut all).await.unwrap();
        assert_eq!(all, *server.data);
        assert_eq!(ranges(&server), vec![format!("bytes=0-{}", SIZE - 1)]);

        let (url, server) = start(SIZE).await;
        let mut short = head_of(&server.data, 30_000);
        short.len = 80_000;
        let mut src = request(&url, SIZE as u64).with_head(Some(short.clone())).source().await.unwrap();
        let mut all = Vec::new();
        src.read_to_end(&mut all).await.unwrap();
        assert_eq!(all, *server.data);
        assert_eq!(ranges(&server), vec![format!("bytes=30000-{}", SIZE - 1)]);
        let _ = std::fs::remove_file(short.path);
    }

    /// The synchronous side (songbird's threads, no Tokio context) over a head: identical bytes,
    /// seeks inside and outside the head.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_ahead_over_a_head_on_a_plain_thread() {
        const SIZE: usize = 600_000;
        let (url, server) = start(SIZE).await;
        let head = head_of(&server.data, 200_000);
        let req = request(&url, SIZE as u64).with_head(Some(head.clone()));
        let AudioStream { input: mut stream, .. } = req.clone().create_async().await.unwrap();
        let data = server.data.clone();
        on_plain_thread(move || {
            assert_eq!(stream.byte_len(), Some(SIZE as u64));
            let mut all = Vec::new();
            stream.read_to_end(&mut all).unwrap();
            assert_eq!(all, *data);
            let mut buf = vec![0u8; 1000];
            assert_eq!(stream.seek(SeekFrom::Start(150_000)).unwrap(), 150_000);
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(buf, data[150_000..151_000]);
            assert_eq!(stream.seek(SeekFrom::Start(400_000)).unwrap(), 400_000);
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(buf, data[400_000..401_000]);
        })
        .await
        .unwrap();
        // Both seeks land in bytes already at hand (the head, then the last bytes the body sent).
        assert_eq!(ranges(&server), vec![format!("bytes=200000-{}", SIZE - 1)]);
        let _ = std::fs::remove_file(head.path);
    }

    /// The seeks symphonia's WebM probe does (seen live: 0, 116, 175, 259, 48), through the sync
    /// side on a plain thread; ends at byte 112.
    fn probe_like(stream: &mut Box<dyn MediaSource>, data: &[u8]) {
        let mut buf = vec![0u8; 4096];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(buf, data[..4096]);
        for at in [116u64, 175, 259, 48] {
            assert_eq!(stream.seek(SeekFrom::Start(at)).unwrap(), at);
            let mut small = [0u8; 64];
            stream.read_exact(&mut small).unwrap();
            assert_eq!(small, data[at as usize..at as usize + 64]);
        }
        // Let the read-ahead fill up.
        std::thread::sleep(Duration::from_millis(300));
    }

    /// With a head, the probe makes no request at all; the network starts once, where the head
    /// ends; a far seek still opens a new range.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_seeks_in_a_head_make_no_request() {
        const SIZE: usize = 2_000_000;
        // Longer than the read-ahead, so nothing needs the network while probing.
        const HEAD: usize = 600_000;
        let (url, server) = start(SIZE).await;
        let head = head_of(&server.data, HEAD);
        let req = request(&url, SIZE as u64).with_head(Some(head.clone()));
        let AudioStream { input: mut stream, .. } = req.clone().create_async().await.unwrap();
        let (data, s) = (server.data.clone(), server.clone());
        on_plain_thread(move || {
            probe_like(&mut stream, &data);
            assert_eq!(ranges(&s), Vec::<String>::new(), "the probe is served from the head");
            let mut more = vec![0u8; 700_000 - 112];
            stream.read_exact(&mut more).unwrap();
            assert_eq!(more, data[112..700_000]);
            let last = SIZE - 1;
            assert_eq!(ranges(&s), vec![format!("bytes={HEAD}-{last}")]);
            let mut rest = Vec::new();
            stream.read_to_end(&mut rest).unwrap();
            assert_eq!(rest, data[700_000..]);
            assert_eq!(ranges(&s), vec![format!("bytes={HEAD}-{last}")]);
            assert_eq!(stream.seek(SeekFrom::Start(700_000)).unwrap(), 700_000);
            let mut buf = vec![0u8; 1000];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(buf, data[700_000..701_000]);
            assert_eq!(
                ranges(&s),
                vec![format!("bytes={HEAD}-{last}"), format!("bytes=700000-{last}")]
            );
        })
        .await
        .unwrap();
        let _ = std::fs::remove_file(head.path);
    }

    /// Without a head, the probe and the whole track need exactly one request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probe_seeks_without_a_head_make_one_request() {
        const SIZE: usize = 2_000_000;
        let (url, server) = start(SIZE).await;
        let req = request(&url, SIZE as u64);
        let AudioStream { input: mut stream, .. } = req.clone().create_async().await.unwrap();
        let (data, s) = (server.data.clone(), server.clone());
        on_plain_thread(move || {
            probe_like(&mut stream, &data);
            let last = SIZE - 1;
            assert_eq!(ranges(&s), vec![format!("bytes=0-{last}")]);
            let mut rest = Vec::new();
            stream.read_to_end(&mut rest).unwrap();
            assert_eq!(rest, data[112..]);
            assert_eq!(ranges(&s), vec![format!("bytes=0-{last}")]);
            assert_eq!(stream.seek(SeekFrom::Start(1_000_000)).unwrap(), 1_000_000);
            let mut buf = vec![0u8; 1000];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(buf, data[1_000_000..1_001_000]);
            assert_eq!(
                ranges(&s),
                vec![format!("bytes=0-{last}"), format!("bytes=1000000-{last}")]
            );
        })
        .await
        .unwrap();
    }

    /// Live: a real YouTube stream through songbird/symphonia, seeking forward 60 s and back to 5 s.
    /// `cargo test -- --ignored live_youtube_seek --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn live_youtube_seek() {
        use std::time::{Duration, Instant};
        use symphonia::core::formats::{SeekMode, SeekTo};
        use symphonia::core::units::Time;

        use crate::{
            procs::{ProcOwner, ProcessRegistry, StreamDirs},
            sources::ytdlp::Ytdlp,
        };

        let root = std::env::temp_dir().join(format!("lynx-seek-{}", uuid::Uuid::new_v4()));
        let http = reqwest::Client::new();
        let y = Ytdlp::new(
            root.join("bin"),
            ProcessRegistry::new(),
            StreamDirs::new(root.join("streams"), 100),
            http.clone(),
            2,
            None,
        );
        let owner = ProcOwner::new(1, 2, Some(uuid::Uuid::new_v4()));
        let t = Instant::now();
        // resolve_stream waits for `available_at`.
        let stream = y
            .resolve_stream("https://www.youtube.com/watch?v=dQw4w9WgXcQ", "dQw4w9WgXcQ", &owner)
            .await
            .unwrap();
        println!(
            "resolved in {:?} (opus={}, available_at={:?})",
            t.elapsed(),
            stream.is_opus,
            stream.available_at
        );
        let input = super::super::playback::http_input(http, &stream, None);
        assert!(matches!(input, Input::Lazy(_)));

        let t = Instant::now();
        let input = input
            .make_playable_async(
                songbird::input::codecs::get_codec_registry(),
                songbird::input::codecs::get_probe(),
            )
            .await
            .unwrap();
        println!("playable in {:?}", t.elapsed());
        let Input::Live(songbird::input::LiveInput::Parsed(mut parsed), _) = input else {
            panic!("not parsed");
        };
        assert!(parsed.supports_backseek);
        let elapsed = on_plain_thread(move || {
            let mut times = Vec::new();
            for secs in [60.0, 5.0, 120.0, 0.5] {
                let t = Instant::now();
                let to = SeekTo::Time {
                    time: Time::from(secs),
                    track_id: Some(parsed.track_id),
                };
                let seeked = parsed.format.seek(SeekMode::Accurate, to).unwrap();
                parsed.decoder.reset();
                // Decode a few packets after the seek.
                for _ in 0..20 {
                    let packet = parsed.format.next_packet().unwrap();
                    if packet.track_id() == parsed.track_id {
                        parsed.decoder.decode(&packet).unwrap();
                    }
                }
                times.push((secs, seeked.actual_ts, t.elapsed()));
            }
            times
        })
        .await
        .unwrap();
        for (secs, ts, took) in elapsed {
            println!("seek to {secs}s -> ts {ts} in {took:?}");
            assert!(took < Duration::from_secs(10));
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
