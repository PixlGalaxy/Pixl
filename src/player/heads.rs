//! Prefetched stream heads: the first ~10 s of an upcoming track's audio, saved on disk in the
//! track's stream dir (`DATA_DIR/streams/{guild}/{channel}/{track}/head-{format}-{len}.bin`), so
//! the track starts (and is probed) without waiting for the network. See `http_seek` for how the
//! input reads the file and then continues with range requests.
//!
//! A head is valid for one exact encode: the key is the played video (or page URL), the format
//! (itag) and the byte length. A re-resolved URL with the same key reuses the file; another
//! format or length does not. The index is process-wide, so any channel playing the same encode
//! can open the file read-only — that is how jam members use the heads the host downloaded.
//! Files go away with their track dir (track played, removed, skipped past, player left, startup
//! wipe, stream cache limit); stale index entries are dropped when a lookup finds no file.

use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt;
use reqwest::Client;
use tokio::{io::AsyncWriteExt, sync::Semaphore};
use tracing::debug;

use super::{
    http_seek::{fetch_range, HeadFile},
    playback::{content_length, stream_headers},
};
use crate::sources::StreamInfo;

/// Audio a head should hold.
pub const HEAD_SECONDS: u64 = 10;
/// Head size when the duration is unknown (~13 s of 160 kbps Opus).
const FALLBACK_HEAD_BYTES: u64 = 256 * 1024;
const MIN_HEAD_BYTES: u64 = 64 * 1024;
const MAX_HEAD_BYTES: u64 = 512 * 1024;
/// Extra bytes for the container header and cues at the start of the file.
const HEADER_SLACK: u64 = 16 * 1024;
/// Head downloads at once, for the whole process (they are background work).
const PARALLEL_DOWNLOADS: usize = 2;
/// A whole head download (one bounded range request) gives up after this long.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// The URL may not be requested before `available_at`; a prefetch waits at most this long for it.
const MAX_AVAILABILITY_WAIT: Duration = Duration::from_secs(20);
/// Index entries kept before dead ones are pruned.
const INDEX_MAX: usize = 1024;

/// One exact encode of a track.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HeadKey {
    /// Played video id (or the page URL for other sites).
    pub id: String,
    /// Format / itag.
    pub format: String,
    /// Byte length of the whole stream.
    pub len: u64,
}

impl HeadKey {
    /// The key of `stream` when it can have a head: a direct HTTP stream of known length.
    pub fn of(id: &str, stream: &StreamInfo) -> Option<Self> {
        if stream.needs_ytdlp {
            return None;
        }
        let len = content_length(&stream.url)?;
        let format = stream
            .format_id
            .clone()
            .or_else(|| url_param(&stream.url, "itag"))
            .unwrap_or_else(|| "x".into());
        Some(Self {
            id: id.to_owned(),
            format,
            len,
        })
    }

    fn file_name(&self) -> String {
        let format: String = self
            .format
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
            .take(32)
            .collect();
        format!("head-{format}-{}.bin", self.len)
    }
}

fn url_param(url: &str, key: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let v = parsed.query_pairs().find(|(k, _)| k == key)?.1.into_owned();
    Some(v)
}

/// Bytes for ~[`HEAD_SECONDS`] of a stream of `total` bytes lasting `duration_ms`.
pub fn head_bytes(total: u64, duration_ms: Option<u64>) -> u64 {
    let n = match duration_ms.filter(|d| *d > 0) {
        Some(d) => (u128::from(total) * u128::from(HEAD_SECONDS * 1000) / u128::from(d)) as u64 + HEADER_SLACK,
        None => FALLBACK_HEAD_BYTES,
    };
    n.clamp(MIN_HEAD_BYTES, MAX_HEAD_BYTES).min(total)
}

/// Index of the complete head files on disk, and the downloads.
#[derive(Clone, Default)]
pub struct Heads {
    inner: Arc<Inner>,
}

struct Inner {
    index: Mutex<HashMap<HeadKey, HeadFile>>,
    busy: Mutex<HashSet<HeadKey>>,
    downloads: Semaphore,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            index: Mutex::default(),
            busy: Mutex::default(),
            downloads: Semaphore::new(PARALLEL_DOWNLOADS),
        }
    }
}

/// How [`Heads::fetch`] went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// Downloaded now.
    Downloaded(HeadFile),
    /// A head of this encode was already on disk (another track dir or channel may own it).
    Existing(HeadFile),
    /// Another download of this encode is running.
    Busy,
}

impl Heads {
    pub fn new() -> Self {
        Self::default()
    }

    /// The complete head of `key`, if one is on disk.
    pub async fn lookup(&self, key: &HeadKey) -> Option<HeadFile> {
        let head = self.inner.index.lock().unwrap().get(key).cloned()?;
        match tokio::fs::metadata(&head.path).await {
            Ok(m) if m.len() == head.len => Some(head),
            _ => {
                self.inner.index.lock().unwrap().remove(key);
                None
            }
        }
    }

    /// Make sure a head of `key` (the encode `stream` points at) is on disk: reuse one, or download
    /// it into `dir` with one bounded range request (after `available_at`, a few at a time).
    pub async fn fetch(&self, client: &Client, stream: &StreamInfo, key: &HeadKey, dir: &Path) -> io::Result<Fetched> {
        if let Some(h) = self.lookup(key).await {
            return Ok(Fetched::Existing(h));
        }
        if !self.inner.busy.lock().unwrap().insert(key.clone()) {
            return Ok(Fetched::Busy);
        }
        let _busy = BusyGuard {
            inner: self.inner.clone(),
            key: key.clone(),
        };
        wait_until_available(stream).await?;
        let _permit = self.inner.downloads.acquire().await.map_err(io::Error::other)?;
        // Another channel may have finished it while this one waited.
        if let Some(h) = self.lookup(key).await {
            return Ok(Fetched::Existing(h));
        }
        let n = head_bytes(key.len, stream.duration_ms);
        let path = dir.join(key.file_name());
        let part = path.with_extension("part");
        let started = std::time::Instant::now();
        let res = tokio::time::timeout(DOWNLOAD_TIMEOUT, download(client, stream, key.len, n, dir, &part)).await;
        let res = match res {
            Ok(r) => r,
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, format!("head download took over {DOWNLOAD_TIMEOUT:?}"))),
        };
        if let Err(e) = res {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
        tokio::fs::rename(&part, &path).await?;
        let head = HeadFile { path, len: n };
        debug!(id = %key.id, format = %key.format, bytes = n, took_ms = started.elapsed().as_millis() as u64, "stream head saved");
        self.insert(key.clone(), head.clone());
        Ok(Fetched::Downloaded(head))
    }

    fn insert(&self, key: HeadKey, head: HeadFile) {
        let mut index = self.inner.index.lock().unwrap();
        if index.len() >= INDEX_MAX {
            index.retain(|_, h| h.path.exists());
            if index.len() >= INDEX_MAX {
                index.clear();
            }
        }
        index.insert(key, head);
    }

    /// Forget the heads under `dir` (a track or channel dir that is being removed).
    pub fn forget_dir(&self, dir: &Path) {
        self.inner.index.lock().unwrap().retain(|_, h| !h.path.starts_with(dir));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.index.lock().unwrap().len()
    }

    /// Register an existing file (tests).
    #[cfg(test)]
    pub fn insert_for_test(&self, key: HeadKey, path: PathBuf, len: u64) {
        self.insert(key, HeadFile { path, len });
    }
}

struct BusyGuard {
    inner: Arc<Inner>,
    key: HeadKey,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.inner.busy.lock().unwrap().remove(&self.key);
    }
}

/// Sleep until the URL may be requested (requesting it earlier gets it refused for good).
async fn wait_until_available(stream: &StreamInfo) -> io::Result<()> {
    let Some(at) = stream.available_at else { return Ok(()) };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let wait_ms = at * 1000 + 750 - now_ms;
    if at * 1000 <= now_ms || wait_ms <= 0 {
        return Ok(());
    }
    let wait = Duration::from_millis(wait_ms as u64);
    if wait > MAX_AVAILABILITY_WAIT {
        return Err(io::Error::other("the stream is not available yet"));
    }
    tokio::time::sleep(wait).await;
    Ok(())
}

/// `bytes=0-(n-1)` of the stream into `part`.
async fn download(client: &Client, stream: &StreamInfo, total: u64, n: u64, dir: &Path, part: &PathBuf) -> io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let mut body = fetch_range(
        client.clone(),
        stream.url.clone(),
        stream_headers(stream),
        total,
        0,
        n.saturating_sub(1),
    )
    .await?;
    let mut file = tokio::fs::File::create(part).await?;
    let mut written: u64 = 0;
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        let take = (n - written).min(chunk.len() as u64) as usize;
        file.write_all(&chunk[..take]).await?;
        written += take as u64;
        if written >= n {
            break;
        }
    }
    file.flush().await?;
    if written != n {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("the head ended after {written} of {n} bytes"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Mutex as StdMutex};

    use axum::{extract::State, http::HeaderMap as AxumHeaders, response::IntoResponse, routing::get, Router};

    use super::*;

    #[derive(Clone)]
    struct Server {
        data: Arc<Vec<u8>>,
        ranges: Arc<StdMutex<Vec<String>>>,
    }

    async fn serve(State(s): State<Server>, headers: AxumHeaders) -> axum::response::Response {
        let range = headers.get("range").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
        s.ranges.lock().unwrap().push(range.clone());
        let len = s.data.len() as u64;
        let Some((a, b)) = range.strip_prefix("bytes=").and_then(|r| r.split_once('-')) else {
            return axum::http::StatusCode::FORBIDDEN.into_response();
        };
        let (a, b): (u64, u64) = (a.parse().unwrap(), b.parse().unwrap());
        if a > b || b >= len {
            return axum::http::StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        }
        let body = s.data[a as usize..=b as usize].to_vec();
        (
            axum::http::StatusCode::PARTIAL_CONTENT,
            [("content-range", format!("bytes {a}-{b}/{len}"))],
            body,
        )
            .into_response()
    }

    async fn start(size: usize) -> (String, Server) {
        let server = Server {
            data: Arc::new((0..size).map(|i| (i % 251) as u8).collect()),
            ranges: Arc::default(),
        };
        let app = Router::new().route("/videoplayback", get(serve)).with_state(server.clone());
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/videoplayback?itag=251&clen={size}"), server)
    }

    fn stream(url: &str, duration_ms: Option<u64>) -> StreamInfo {
        StreamInfo {
            url: url.to_owned(),
            http_headers: Vec::new(),
            expires_at: i64::MAX,
            container: Some("webm".into()),
            codec: Some("opus".into()),
            format_id: Some("251".into()),
            protocol: Some("https".into()),
            is_opus: true,
            needs_ytdlp: false,
            duration_ms,
            available_at: None,
        }
    }

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("lynx-heads-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn head_size_is_about_ten_seconds() {
        // 160 kbps Opus, 200 s: 20 kB/s -> 200 kB + slack.
        assert_eq!(head_bytes(4_000_000, Some(200_000)), 200_000 + HEADER_SLACK);
        assert_eq!(head_bytes(4_000_000, None), FALLBACK_HEAD_BYTES);
        // Very high bitrate is capped; tiny files are taken whole.
        assert_eq!(head_bytes(100_000_000, Some(60_000)), MAX_HEAD_BYTES);
        assert_eq!(head_bytes(40_000, Some(60_000)), 40_000);
        assert_eq!(head_bytes(10_000_000, Some(10_000_000)), MIN_HEAD_BYTES);
    }

    #[test]
    fn keys_need_a_known_length_and_tell_encodes_apart() {
        let s = stream("https://x.googlevideo.com/videoplayback?itag=251&clen=1000", None);
        let k = HeadKey::of("abc", &s).unwrap();
        assert_eq!((k.format.as_str(), k.len), ("251", 1000));
        assert_eq!(k.file_name(), "head-251-1000.bin");
        assert!(HeadKey::of("abc", &stream("https://example.com/a.opus", None)).is_none());
        let mut fragmented = s.clone();
        fragmented.needs_ytdlp = true;
        assert!(HeadKey::of("abc", &fragmented).is_none());
        let other = stream("https://y.googlevideo.com/videoplayback?itag=251&clen=1001", None);
        assert_ne!(HeadKey::of("abc", &other), Some(k));
    }

    /// One bounded range request writes exactly the head; a second fetch (another channel, or a
    /// jam member) reuses the file without any request.
    #[tokio::test]
    async fn download_once_then_reuse() {
        const SIZE: usize = 2_000_000;
        let (url, server) = start(SIZE).await;
        let s = stream(&url, Some(100_000));
        let key = HeadKey::of("vid", &s).unwrap();
        let heads = Heads::new();
        let dir = tmp().join("1").join("2").join("track");
        let client = Client::new();
        let got = heads.fetch(&client, &s, &key, &dir).await.unwrap();
        let Fetched::Downloaded(head) = got else { panic!("{got:?}") };
        let n = head_bytes(SIZE as u64, Some(100_000));
        assert_eq!(head.len, n);
        assert_eq!(head.path, dir.join("head-251-2000000.bin"));
        assert_eq!(std::fs::read(&head.path).unwrap(), server.data[..n as usize]);
        assert!(!dir.join("head-251-2000000.part").exists());
        assert_eq!(*server.ranges.lock().unwrap(), vec![format!("bytes=0-{}", n - 1)]);

        // Same encode from another dir: the existing file, no request.
        let other = tmp();
        let again = heads.fetch(&client, &s, &key, &other).await.unwrap();
        assert_eq!(again, Fetched::Existing(head.clone()));
        assert_eq!(server.ranges.lock().unwrap().len(), 1);
        assert_eq!(heads.lookup(&key).await, Some(head.clone()));

        // The dir goes: the head is forgotten.
        heads.forget_dir(&dir);
        assert!(heads.lookup(&key).await.is_none());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap().parent().unwrap());
    }

    /// A head whose file disappeared (or changed size) is not used.
    #[tokio::test]
    async fn lookup_checks_the_file() {
        let heads = Heads::new();
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("head-251-10.bin");
        std::fs::write(&path, [0u8; 5]).unwrap();
        let key = HeadKey {
            id: "a".into(),
            format: "251".into(),
            len: 10,
        };
        heads.insert_for_test(key.clone(), path.clone(), 4);
        assert!(heads.lookup(&key).await.is_none(), "wrong size");
        assert_eq!(heads.len(), 0, "stale entries are dropped");
        heads.insert_for_test(key.clone(), path.clone(), 5);
        assert!(heads.lookup(&key).await.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(heads.lookup(&key).await.is_none(), "file gone");
    }

    /// A refused range request leaves no file behind.
    #[tokio::test]
    async fn failed_download_leaves_nothing() {
        let (url, server) = start(1000).await;
        // Claim a longer stream than served: the range is out of bounds (416).
        let s = stream(&url.replace("clen=1000", "clen=5000000"), Some(10_000));
        let key = HeadKey::of("vid", &s).unwrap();
        let heads = Heads::new();
        let dir = tmp();
        assert!(heads.fetch(&Client::new(), &s, &key, &dir).await.is_err());
        assert!(!dir.join(key.file_name()).exists());
        assert!(!dir.join(key.file_name()).with_extension("part").exists());
        assert!(heads.lookup(&key).await.is_none());
        assert_eq!(server.ranges.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
