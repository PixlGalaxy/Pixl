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

use std::{
    io::{self, Read, Seek, SeekFrom},
    pin::Pin,
    sync::Mutex,
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
}

type Body = BoxStream<'static, io::Result<Bytes>>;

impl RangeRequest {
    /// `GET` with `Range: bytes=at-(len-1)`, answered within `timeouts.open`. Only a 206 whose
    /// `Content-Range` starts at `at` is accepted: anything else would put the demuxer at the
    /// wrong byte. The body fails when it sends nothing for `timeouts.stall`.
    async fn open(self, at: u64) -> io::Result<Body> {
        let limit = self.timeouts.open;
        let len = self.len;
        let started = std::time::Instant::now();
        let res = match tokio::time::timeout(limit, self.open_now(at)).await {
            Ok(res) => res,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("range request from byte {at} got no answer in {limit:?}"),
            )),
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &res {
            Ok(_) => debug!(from = at, len, elapsed_ms, "stream range request answered"),
            Err(e) => warn!(from = at, len, elapsed_ms, error = %e, "stream range request failed"),
        }
        res
    }

    async fn open_now(self, at: u64) -> io::Result<Body> {
        let last = self.len.saturating_sub(1);
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
    body: Option<Body>,
    /// A range request in flight, with the byte it starts at.
    opening: Option<(u64, BoxFuture<'static, io::Result<Body>>)>,
}

/// Async, seekable HTTP stream of a known length. Reads advance `pos`; a seek to another position
/// replaces the body with a new range request. A broken body is retried once from `pos`.
pub struct SeekableHttpSource {
    req: RangeRequest,
    pos: u64,
    /// Rest of the last body chunk.
    chunk: Bytes,
    seek_to: Option<u64>,
    /// The body broke and was reopened; reset by the next successful chunk.
    retried: bool,
    pending: Mutex<Pending>,
}

impl SeekableHttpSource {
    fn new(req: RangeRequest, pos: u64, body: Option<Body>) -> Self {
        Self {
            req,
            pos,
            chunk: Bytes::new(),
            seek_to: None,
            retried: false,
            pending: Mutex::new(Pending { body, opening: None }),
        }
    }

    fn len(&self) -> u64 {
        self.req.len
    }

    fn pending(&mut self) -> &mut Pending {
        self.pending.get_mut().unwrap_or_else(|p| p.into_inner())
    }

    /// Make sure a range request from `at` is in flight (dropping the current body).
    fn ensure_opening(&mut self, at: u64) {
        let req = self.req.clone();
        let pending = self.pending();
        if !matches!(&pending.opening, Some((from, _)) if *from == at) {
            pending.body = None;
            pending.opening = Some((at, req.open(at).boxed()));
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
            if !this.chunk.is_empty() {
                let n = this.chunk.len().min(buf.remaining());
                buf.put_slice(&this.chunk[..n]);
                this.chunk.advance(n);
                this.pos += n as u64;
                return Poll::Ready(Ok(()));
            }
            if this.pos >= this.len() || buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            let pos = this.pos;
            if this.pending().body.is_none() {
                this.ensure_opening(pos);
            }
            if let Err(e) = std::task::ready!(this.poll_opening(cx)) {
                return Poll::Ready(Err(e));
            }
            let Some(body) = &mut this.pending().body else { continue };
            let err = match std::task::ready!(body.poll_next_unpin(cx)) {
                Some(Ok(mut bytes)) => {
                    // Never hand out more than the announced length.
                    bytes.truncate((this.len() - pos).min(bytes.len() as u64) as usize);
                    this.chunk = bytes;
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
            warn!(error = %err, pos, "stream read failed; reopening it at the current position");
            this.retried = true;
            this.ensure_opening(pos);
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
        this.seek_to = Some(target);
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        let Some(target) = this.seek_to else {
            return Poll::Ready(Ok(this.pos));
        };
        let pos = this.pos;
        // Already there, or inside the chunk we hold.
        if target >= pos && target - pos <= this.chunk.len() as u64 {
            this.chunk.advance((target - pos) as usize);
            this.pos = target;
            this.seek_to = None;
            return Poll::Ready(Ok(target));
        }
        if target >= this.len() {
            let pending = this.pending();
            pending.body = None;
            pending.opening = None;
            this.chunk.clear();
            this.pos = target;
            this.seek_to = None;
            return Poll::Ready(Ok(target));
        }
        this.ensure_opening(target);
        let res = std::task::ready!(this.poll_opening(cx));
        this.seek_to = None;
        // The old body is gone either way; the next read reopens at `pos` after a failure.
        this.chunk.clear();
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
        rt.block_on(tokio::time::timeout(wait, fut)).map_err(|_| {
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
            },
        }
    }

    /// The async source at the start of the stream (first request already answered).
    async fn source(&self) -> io::Result<SeekableHttpSource> {
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
        const SIZE: usize = 300_000;
        let (url, server) = start(SIZE).await;
        let mut src = request(&url, SIZE as u64).source().await.unwrap();
        assert!(AsyncMediaSource::is_seekable(&src));
        assert_eq!(src.byte_len().await, Some(SIZE as u64));

        let mut buf = [0u8; 100];
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[..100]);

        // Forward far away, then backward.
        assert_eq!(src.seek(SeekFrom::Start(200_000)).await.unwrap(), 200_000);
        src.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf.to_vec(), server.data[200_000..200_100]);
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

        let last = SIZE - 1;
        assert_eq!(
            ranges(&server),
            vec![
                format!("bytes=0-{last}"),
                format!("bytes=200000-{last}"),
                format!("bytes=5000-{last}"),
                format!("bytes=5050-{last}"),
                format!("bytes={}-{last}", SIZE - 10),
            ]
        );
        assert!(src.seek(SeekFrom::Current(-(SIZE as i64) - 1)).await.is_err());
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_ahead_waits_a_bounded_time() {
        let mut stream = ReadAhead::with_wait(Box::new(Never), Duration::from_millis(200));
        tokio::task::spawn_blocking(move || {
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
        tokio::task::spawn_blocking(move || {
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
        let last = SIZE - 1;
        assert_eq!(
            ranges(&server),
            vec![
                format!("bytes=0-{last}"),
                format!("bytes=600000-{last}"),
                format!("bytes=20000-{last}"),
                format!("bytes={}-{last}", SIZE - 500),
                format!("bytes=0-{last}"),
            ]
        );
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
        let input = super::super::playback::http_input(http, &stream);
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
        let elapsed = tokio::task::spawn_blocking(move || {
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
