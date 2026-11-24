//! yt-dlp: direct stream URLs (no download), metadata for non-YouTube URLs, and self-update.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use super::{SourceError, SourceResult, TrackInfo, TrackOrigin};
use crate::{
    procs::{ProcError, ProcOwner, ProcSpec, ProcessRegistry, StreamDirs},
    store::now_secs,
};

const FORMAT: &str = "bestaudio[acodec=opus]/bestaudio/best";
/// Executable name (native Windows dev runs use the .exe release).
const BIN_NAME: &str = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);
const STREAM_CACHE_MAX: usize = 2048;
/// Keys remembered as "resolve with yt-dlp's default clients" after a fast-client URL failed.
const DEFAULT_CLIENT_KEYS_MAX: usize = 2048;
/// YouTube player client tried first by default. Measured with yt-dlp 2026.11.23: its URLs carry
/// no future `available_at` (with the default clients googlevideo refuses the URL for 4-14 s
/// after the resolve), it needs no JS challenge, and it still offers itag 251 (Opus in WebM) as
/// one seekable HTTPS file.
pub const DEFAULT_PLAYER_CLIENT: &str = "visionos";
/// A cached URL is reused only if it stays valid at least this long (covers a long song).
pub const STREAM_MIN_REMAINING: i64 = 30 * 60;
/// Assumed lifetime of URLs without an `expire` parameter.
const DEFAULT_URL_TTL: i64 = 60 * 60;
pub const UPDATE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// What songbird needs to read a stream over HTTP.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamInfo {
    pub url: String,
    pub http_headers: Vec<(String, String)>,
    /// Unix seconds.
    pub expires_at: i64,
    /// Container / extension (`webm`, `m4a`...).
    pub container: Option<String>,
    pub codec: Option<String>,
    pub format_id: Option<String>,
    pub protocol: Option<String>,
    /// Opus in WebM (YouTube itag 251): songbird can pass it through without re-encoding.
    pub is_opus: bool,
    /// The stream is not a single progressive HTTP file (HLS/DASH fragments); play it through
    /// yt-dlp itself instead of a direct HTTP read.
    pub needs_ytdlp: bool,
    pub duration_ms: Option<u64>,
    /// Unix seconds before which googlevideo answers 403 (and then rejects the URL for good).
    /// yt-dlp reports it as `available_at`; the URL must not be requested earlier.
    pub available_at: Option<i64>,
}

/// How one stream resolve went, for the start-up timing log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResolveStats {
    /// The URL came from the in-memory stream cache (no yt-dlp run).
    pub cache_hit: bool,
    /// Time slept waiting for YouTube's `available_at`.
    pub waited: Duration,
}

#[derive(Clone)]
pub struct Ytdlp {
    inner: Arc<Inner>,
}

struct Inner {
    bin_dir: PathBuf,
    procs: ProcessRegistry,
    dirs: StreamDirs,
    http: reqwest::Client,
    sem: Semaphore,
    cookies: Option<PathBuf>,
    /// `--limit-rate` for the piped fallback (HLS/live). Direct streams are already paced by songbird's
    /// 64 KiB read-ahead buffer, so they never download faster than they play.
    limit_rate: Option<String>,
    /// yt-dlp's cache (player JS, challenge solutions). `None`: yt-dlp's own default.
    cache_dir: Option<PathBuf>,
    /// `youtube:player_client=` tried first for stream resolves; when it fails the resolve is
    /// repeated with yt-dlp's default clients. `None`: always yt-dlp's default.
    player_client: Option<String>,
    cache: Mutex<HashMap<String, StreamInfo>>,
    /// One yt-dlp resolve per cache key at a time: concurrent resolves of the same video (two
    /// jam channels, a warm-up and the player) wait for the first and take its cached result.
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Keys whose stream URL was invalidated (refused, failed): resolved with the default clients.
    default_client_keys: Mutex<HashSet<String>>,
    update_lock: tokio::sync::Mutex<()>,
    /// Admin panel switch: the periodic self-update is skipped (a manual check still runs).
    updates_paused: std::sync::atomic::AtomicBool,
}

impl Ytdlp {
    pub fn new(
        bin_dir: PathBuf,
        procs: ProcessRegistry,
        dirs: StreamDirs,
        http: reqwest::Client,
        max_concurrent: usize,
        cookies: Option<PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                bin_dir,
                procs,
                dirs,
                http,
                sem: Semaphore::new(max_concurrent.max(1)),
                cookies,
                limit_rate: None,
                cache_dir: None,
                player_client: None,
                cache: Mutex::new(HashMap::new()),
                inflight: Mutex::new(HashMap::new()),
                default_client_keys: Mutex::new(HashSet::new()),
                update_lock: tokio::sync::Mutex::new(()),
                updates_paused: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// Cap the download rate of the piped fallback (e.g. "300K"). Call right after `new`.
    pub fn with_limit_rate(mut self, rate: Option<String>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.limit_rate = rate.filter(|r| !r.is_empty());
        }
        self
    }

    /// Persistent yt-dlp cache dir, so player JS and challenge solutions survive restarts instead
    /// of being fetched and solved again. Call right after `new`.
    pub fn with_cache_dir(mut self, dir: Option<PathBuf>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.cache_dir = dir;
        }
        self
    }

    /// YouTube player client(s) tried first for stream resolves (comma-separated, as yt-dlp's
    /// `player_client` extractor arg). Empty or `default`: yt-dlp's default only. Call right after `new`.
    pub fn with_player_client(mut self, client: Option<String>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.player_client = client
                .map(|c| c.trim().to_owned())
                .filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("default"));
        }
        self
    }

    /// `DATA_DIR/bin/yt-dlp` when present, else `yt-dlp` from PATH.
    pub fn binary(&self) -> PathBuf {
        let local = self.inner.bin_dir.join(BIN_NAME);
        if local.is_file() {
            local
        } else {
            PathBuf::from(BIN_NAME)
        }
    }

    fn base_args(&self, work_dir: &Path) -> Vec<String> {
        let mut a = vec![
            "--ignore-config".into(),
            "--no-warnings".into(),
            "--no-progress".into(),
            "--paths".into(),
            format!("temp:{}", work_dir.display()),
        ];
        if let Some(dir) = &self.inner.cache_dir {
            a.push("--cache-dir".into());
            a.push(dir.display().to_string());
        }
        if let Some(c) = &self.inner.cookies {
            a.push("--cookies".into());
            a.push(c.display().to_string());
        }
        a
    }

    /// Command that pipes the audio of `page_url` to stdout (fragmented or live streams), run in
    /// `work_dir`. Spawn it with `ProcessRegistry::spawn_stream`.
    pub fn stream_spec(&self, page_url: &str, owner: &ProcOwner, work_dir: &Path, max_duration: Duration) -> ProcSpec {
        let mut args = self.base_args(work_dir);
        if let Some(rate) = &self.inner.limit_rate {
            args.extend(["--limit-rate".into(), rate.clone()]);
        }
        args.extend(["--quiet".into(), "-f".into(), FORMAT.into(), "-o".into(), "-".into(), page_url.into()]);
        ProcSpec::new(self.binary(), owner.clone(), max_duration).args(args).current_dir(work_dir)
    }

    /// Run yt-dlp under the semaphore, in the owner's stream dir. Returns stdout.
    async fn run(&self, owner: &ProcOwner, args: Vec<String>, timeout: Duration) -> SourceResult<Vec<u8>> {
        let _permit = self.inner.sem.acquire().await.map_err(|_| SourceError::Other("shutting down".into()))?;
        let dir = self.inner.dirs.create_for(owner).await.map_err(|e| SourceError::Other(e.to_string()))?;
        let mut full = self.base_args(&dir);
        full.extend(args);
        let spec = ProcSpec::new(self.binary(), owner.clone(), timeout).args(full).current_dir(&dir);
        let res = self.inner.procs.run(spec).await;
        if owner.track_uid.is_none() {
            // One-off dir; track dirs are removed by the player when the track ends.
            let _ = tokio::fs::remove_dir_all(&dir).await;
        }
        match res {
            Ok(out) if out.status.success() => Ok(out.stdout),
            Ok(out) => Err(SourceError::Unavailable(short_reason(&out.stderr))),
            Err(ProcError::Timeout(..)) => Err(SourceError::Timeout),
            Err(ProcError::Killed(_)) => Err(SourceError::Cancelled),
            Err(e) => Err(SourceError::Other(e.to_string())),
        }
    }

    /// Resolve the direct audio URL. `cache_key` is the YouTube video id (or the URL for others).
    pub async fn resolve_stream(&self, url: &str, cache_key: &str, owner: &ProcOwner) -> SourceResult<StreamInfo> {
        self.resolve_stream_stats(url, cache_key, owner).await.map(|(info, _)| info)
    }

    /// [`Self::resolve_stream`], also telling whether the URL came from the cache and how long
    /// the `available_at` wait slept (for the start-up timing log).
    pub async fn resolve_stream_stats(
        &self,
        url: &str,
        cache_key: &str,
        owner: &ProcOwner,
    ) -> SourceResult<(StreamInfo, ResolveStats)> {
        if let Some(hit) = self.cached(cache_key) {
            let waited = wait_until_available(&hit).await;
            return Ok((hit, ResolveStats { cache_hit: true, waited }));
        }
        let flight = self.flight(cache_key).await;
        // Another caller resolved it while this one waited.
        if let Some(hit) = self.cached(cache_key) {
            drop(flight);
            let waited = wait_until_available(&hit).await;
            return Ok((hit, ResolveStats { cache_hit: true, waited }));
        }
        let info = self.fetch_stream(url, cache_key, owner).await?;
        self.store(cache_key, info.clone());
        drop(flight);
        let waited = wait_until_available(&info).await;
        Ok((info, ResolveStats { cache_hit: false, waited }))
    }

    /// Resolve `url` into the stream cache without waiting for `available_at`, so the player's
    /// [`Self::resolve_stream`] finds it (or waits for this run instead of starting its own).
    /// Errors only log: the player's own resolve reports them.
    pub async fn warm(&self, url: &str, cache_key: &str, owner: &ProcOwner) {
        if self.cached(cache_key).is_some() {
            return;
        }
        let _flight = self.flight(cache_key).await;
        if self.cached(cache_key).is_some() {
            return;
        }
        match self.fetch_stream(url, cache_key, owner).await {
            Ok(info) => self.store(cache_key, info),
            Err(e) => tracing::debug!(key = cache_key, error = %e, "stream warm-up failed"),
        }
    }

    /// Wait for any resolve of `key` already running, then hold the key until the guard drops.
    async fn flight(&self, key: &str) -> Flight {
        let lock = self.inner.inflight.lock().unwrap().entry(key.to_owned()).or_default().clone();
        let guard = lock.clone().lock_owned().await;
        Flight {
            inner: self.inner.clone(),
            key: key.to_owned(),
            lock,
            guard: Some(guard),
        }
    }

    /// One stream resolve: the fast player client first (when configured and not ruled out for
    /// this key), then yt-dlp's default clients.
    async fn fetch_stream(&self, url: &str, cache_key: &str, owner: &ProcOwner) -> SourceResult<StreamInfo> {
        let fast = self
            .inner
            .player_client
            .as_deref()
            .filter(|_| !self.inner.default_client_keys.lock().unwrap().contains(cache_key));
        if let Some(client) = fast {
            match self.run_resolve(url, Some(client), owner).await {
                Ok(info) => return Ok(info),
                Err(e) if !retry_with_default_client(&e) => return Err(e),
                Err(e) => debug!(key = cache_key, client, error = %e, "fast player client failed; trying the default clients"),
            }
        }
        self.run_resolve(url, None, owner).await
    }

    async fn run_resolve(&self, url: &str, client: Option<&str>, owner: &ProcOwner) -> SourceResult<StreamInfo> {
        let template = "%(.{id,url,http_headers,acodec,ext,format_id,protocol,duration,is_live,available_at})j";
        let mut args: Vec<String> = Vec::new();
        if let Some(c) = client {
            args.extend(["--extractor-args".into(), format!("youtube:player_client={c}")]);
        }
        args.extend([
            "--no-playlist".into(),
            "-f".into(),
            FORMAT.into(),
            "-O".into(),
            template.into(),
            "--".into(),
            url.into(),
        ]);
        let out = self.run(owner, args, RESOLVE_TIMEOUT).await?;
        let line = String::from_utf8_lossy(&out);
        let line = line.lines().rev().find(|l| l.trim_start().starts_with('{')).unwrap_or("");
        let v: Value =
            serde_json::from_str(line).map_err(|_| SourceError::Other("yt-dlp returned no stream info".into()))?;
        parse_stream(&v, now_secs()).ok_or_else(|| SourceError::Unavailable("No audio stream".into()))
    }

    fn cached(&self, key: &str) -> Option<StreamInfo> {
        let cache = self.inner.cache.lock().unwrap();
        cache.get(key).filter(|s| s.expires_at - now_secs() > STREAM_MIN_REMAINING).cloned()
    }

    fn store(&self, key: &str, info: StreamInfo) {
        let mut cache = self.inner.cache.lock().unwrap();
        if cache.len() >= STREAM_CACHE_MAX {
            let now = now_secs();
            cache.retain(|_, s| s.expires_at - now > STREAM_MIN_REMAINING);
            if cache.len() >= STREAM_CACHE_MAX {
                // Still full: drop the entries closest to expiry.
                let mut by_exp: Vec<(String, i64)> = cache.iter().map(|(k, s)| (k.clone(), s.expires_at)).collect();
                by_exp.sort_by_key(|(_, e)| *e);
                for (k, _) in by_exp.into_iter().take(STREAM_CACHE_MAX / 4) {
                    cache.remove(&k);
                }
            }
        }
        cache.insert(key.to_owned(), info);
    }

    /// Forget the cached URL of `key` (refused, or the track failed). Its next resolve skips the
    /// fast player client, in case that client's URL is what went wrong.
    pub fn invalidate(&self, key: &str) {
        self.inner.cache.lock().unwrap().remove(key);
        if self.inner.player_client.is_some() {
            let mut keys = self.inner.default_client_keys.lock().unwrap();
            if keys.len() >= DEFAULT_CLIENT_KEYS_MAX {
                keys.clear();
            }
            keys.insert(key.to_owned());
        }
    }

    /// Forget every cached stream URL (admin panel); the next play resolves again.
    pub fn clear_stream_cache(&self) -> usize {
        let mut cache = self.inner.cache.lock().unwrap();
        let n = cache.len();
        cache.clear();
        self.inner.default_client_keys.lock().unwrap().clear();
        n
    }

    pub fn set_updates_paused(&self, paused: bool) {
        self.inner.updates_paused.store(paused, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn updates_paused(&self) -> bool {
        self.inner.updates_paused.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Metadata for any yt-dlp supported URL (single item or flat playlist).
    pub async fn probe(&self, url: &str, limit: usize, owner: &ProcOwner) -> SourceResult<(Vec<TrackInfo>, Option<String>)> {
        let args = vec![
            "--dump-single-json".into(),
            "--flat-playlist".into(),
            "--playlist-end".into(),
            limit.max(1).to_string(),
            "--".into(),
            url.into(),
        ];
        let out = self.run(owner, args, PROBE_TIMEOUT).await?;
        let v: Value = serde_json::from_slice(&out).map_err(|_| SourceError::Other("yt-dlp returned invalid JSON".into()))?;
        let (mut tracks, name) = parse_probe(&v);
        tracks.truncate(limit);
        if tracks.is_empty() {
            return Err(SourceError::NotFound("Nothing playable at that URL".into()));
        }
        Ok((tracks, name))
    }

    pub async fn version(&self) -> Option<String> {
        let spec = ProcSpec::new(self.binary(), ProcOwner::system(), VERSION_TIMEOUT).arg("--version");
        let out = self.inner.procs.run(spec).await.ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|v| !v.is_empty())
    }

    /// Download the latest standalone yt-dlp into `bin/` if it differs from the current one.
    /// Returns the new version when an update was installed.
    pub async fn update(&self) -> anyhow::Result<Option<String>> {
        let _guard = self.inner.update_lock.lock().await;
        let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("windows", _) => "yt-dlp.exe",
            ("macos", _) => "yt-dlp_macos",
            (_, "aarch64") => "yt-dlp_linux_aarch64",
            _ => "yt-dlp_linux",
        };
        let rel: Value = self
            .inner
            .http
            .get("https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest")
            .header(reqwest::header::USER_AGENT, super::USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .timeout(Duration::from_secs(30))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let latest = rel["tag_name"].as_str().unwrap_or("").trim().to_owned();
        anyhow::ensure!(!latest.is_empty(), "GitHub release has no tag");
        let current = self.version().await;
        if current.as_deref() == Some(latest.as_str()) {
            return Ok(None);
        }
        let url = rel["assets"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(asset)))
            .and_then(|x| x["browser_download_url"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("https://github.com/yt-dlp/yt-dlp/releases/download/{latest}/{asset}"));

        tokio::fs::create_dir_all(&self.inner.bin_dir).await?;
        let tmp = self.inner.bin_dir.join(format!(".yt-dlp.{}.tmp", uuid::Uuid::new_v4()));
        let result = self.download_to(&url, &tmp).await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        // Sanity check the new binary before swapping it in.
        let spec = ProcSpec::new(&tmp, ProcOwner::system(), VERSION_TIMEOUT).arg("--version");
        let ok = matches!(self.inner.procs.run(spec).await, Ok(o) if o.status.success());
        if !ok {
            let _ = tokio::fs::remove_file(&tmp).await;
            anyhow::bail!("downloaded yt-dlp does not run");
        }
        tokio::fs::rename(&tmp, self.inner.bin_dir.join(BIN_NAME)).await?;
        info!(from = ?current, to = %latest, "yt-dlp updated");
        Ok(Some(latest))
    }

    async fn download_to(&self, url: &str, path: &Path) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut res = self
            .inner
            .http
            .get(url)
            .header(reqwest::header::USER_AGENT, super::USER_AGENT)
            .timeout(Duration::from_secs(300))
            .send()
            .await?
            .error_for_status()?;
        let mut f = tokio::fs::File::create(path).await?;
        let mut n = 0usize;
        while let Some(chunk) = res.chunk().await? {
            n += chunk.len();
            f.write_all(&chunk).await?;
        }
        f.flush().await?;
        f.sync_all().await?;
        drop(f);
        anyhow::ensure!(n > 1024 * 1024, "download too small ({n} bytes)");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).await?;
        }
        Ok(())
    }

    /// Update at startup and every 24 h. Spawn the returned future; it ends on `stop`.
    pub fn update_task(
        &self,
        stop: tokio_util::sync::CancellationToken,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let this = self.clone();
        async move {
            let mut tick = tokio::time::interval(UPDATE_EVERY);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tick.tick() => {}
                }
                if this.updates_paused() {
                    info!("yt-dlp self-update is paused (admin panel)");
                    continue;
                }
                match this.update().await {
                    Ok(Some(_)) => {}
                    Ok(None) => info!("yt-dlp is up to date"),
                    Err(e) => warn!(error = %e, "yt-dlp self-update failed"),
                }
            }
        }
    }
}

/// Holds a key of [`Inner::inflight`]; the entry is removed once nobody else waits on it.
struct Flight {
    inner: Arc<Inner>,
    key: String,
    lock: Arc<tokio::sync::Mutex<()>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for Flight {
    fn drop(&mut self) {
        self.guard.take();
        let mut map = self.inner.inflight.lock().unwrap_or_else(|p| p.into_inner());
        // Only the map's clone and ours left: no waiter.
        if map.get(&self.key).is_some_and(|l| Arc::ptr_eq(l, &self.lock) && Arc::strong_count(l) <= 2) {
            map.remove(&self.key);
        }
    }
}

/// A failed fast-client resolve is repeated with yt-dlp's default clients unless that cannot
/// help (cancelled, timed out, or the video is private).
fn retry_with_default_client(e: &SourceError) -> bool {
    match e {
        SourceError::Cancelled | SourceError::Timeout => false,
        SourceError::Unavailable(r) => r != "Private video",
        _ => true,
    }
}

/// Last meaningful error from yt-dlp stderr, shortened for users and logs.
pub fn short_reason(stderr: &str) -> String {
    const KNOWN: &[(&str, &str)] = &[
        ("sign in to confirm you", "Sign in to confirm you're not a bot"),
        ("sign in to confirm your age", "Age-restricted video"),
        ("age-restricted", "Age-restricted video"),
        ("private video", "Private video"),
        ("this video is private", "Private video"),
        ("video unavailable", "Video unavailable"),
        ("this video is unavailable", "Video unavailable"),
        ("this video has been removed", "Video unavailable"),
        ("not available in your country", "Not available in this country"),
        ("requested format is not available", "Requested format is not available"),
        ("members-only", "Members-only video"),
        ("join this channel", "Members-only video"),
        ("premieres in", "Video has not premiered yet"),
        ("http error 429", "Rate limited by the site (HTTP 429)"),
        ("http error 403", "Access denied (HTTP 403)"),
        ("http error 404", "Not found (HTTP 404)"),
        ("unsupported url", "Unsupported URL"),
        ("is not a valid url", "Invalid URL"),
        ("unable to download webpage", "Could not reach the site"),
    ];
    let lower = stderr.to_lowercase();
    for (needle, msg) in KNOWN {
        if lower.contains(needle) {
            return (*msg).to_owned();
        }
    }
    let line = stderr
        .lines()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("yt-dlp failed");
    // "ERROR: [youtube] dQw4w9WgXcQ: message" -> "message"
    let mut msg = line.trim_start_matches("ERROR:").trim();
    if msg.starts_with('[') {
        if let Some(end) = msg.find(']') {
            msg = msg[end + 1..].trim();
            if let Some((head, rest)) = msg.split_once(": ") {
                if !head.contains(' ') {
                    msg = rest.trim();
                }
            }
        }
    }
    let mut s: String = msg.chars().take(160).collect();
    if s.is_empty() {
        s = "yt-dlp failed".into();
    }
    s
}

pub(crate) fn parse_stream(v: &Value, now: i64) -> Option<StreamInfo> {
    let url = v["url"].as_str().filter(|u| !u.is_empty())?.to_owned();
    let mut http_headers: Vec<(String, String)> = v["http_headers"]
        .as_object()
        .map(|m| m.iter().filter_map(|(k, x)| x.as_str().map(|s| (k.clone(), s.to_owned()))).collect())
        .unwrap_or_default();
    http_headers.sort();
    let expires_at = reqwest::Url::parse(&url)
        .ok()
        .and_then(|u| u.query_pairs().find(|(k, _)| k == "expire").and_then(|(_, x)| x.parse::<i64>().ok()))
        .unwrap_or(now + DEFAULT_URL_TTL);
    let codec = v["acodec"].as_str().filter(|c| *c != "none").map(str::to_owned);
    let container = v["ext"].as_str().map(str::to_owned);
    let protocol = v["protocol"].as_str().map(str::to_owned);
    let is_opus = codec.as_deref().is_some_and(|c| c.starts_with("opus")) && container.as_deref() == Some("webm");
    let needs_ytdlp = v["is_live"].as_bool() == Some(true)
        || !matches!(protocol.as_deref(), None | Some("https") | Some("http"));
    Some(StreamInfo {
        url,
        http_headers,
        expires_at,
        container,
        codec,
        format_id: v["format_id"].as_str().map(str::to_owned),
        protocol,
        is_opus,
        needs_ytdlp,
        duration_ms: v["duration"].as_f64().map(|d| (d * 1000.0) as u64),
        available_at: v["available_at"].as_i64().or_else(|| v["available_at"].as_f64().map(|f| f.ceil() as i64)),
    })
}

/// Longest wait for a stream to become available (YouTube asks for a few seconds).
const MAX_AVAILABILITY_WAIT: Duration = Duration::from_secs(20);

/// Sleep until the stream's `available_at` (plus a margin), so the first request is not refused.
/// Returns how long it slept.
async fn wait_until_available(info: &StreamInfo) -> Duration {
    let Some(at) = info.available_at else { return Duration::ZERO };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    // Already reached (the usual case with the fast player client): no safety margin. Measured:
    // such URLs answer 206 right away.
    if at * 1000 <= now {
        return Duration::ZERO;
    }
    let wait_ms = (at * 1000 + 750) - now;
    let wait = Duration::from_millis(wait_ms as u64).min(MAX_AVAILABILITY_WAIT);
    tokio::time::sleep(wait).await;
    wait
}

fn thumb_of(v: &Value) -> Option<String> {
    v["thumbnail"].as_str().map(str::to_owned).or_else(|| {
        v["thumbnails"].as_array().and_then(|a| {
            a.iter()
                .max_by_key(|t| t["width"].as_u64().unwrap_or(0) * t["height"].as_u64().unwrap_or(0))
                .and_then(|t| t["url"].as_str().map(str::to_owned))
        })
    })
}

fn entry_to_track(e: &Value) -> Option<TrackInfo> {
    let extractor = e["ie_key"].as_str().or_else(|| e["extractor_key"].as_str()).unwrap_or("");
    let is_yt = extractor.eq_ignore_ascii_case("youtube");
    let id = e["id"].as_str();
    let url = e["webpage_url"]
        .as_str()
        .or_else(|| e["url"].as_str())
        .map(str::to_owned)
        .or_else(|| is_yt.then(|| id.map(|i| format!("https://www.youtube.com/watch?v={i}"))).flatten())?;
    let title = e["track"].as_str().or_else(|| e["title"].as_str()).unwrap_or(&url).to_owned();
    let artists: Vec<String> = e["artists"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect::<Vec<_>>())
        .filter(|a| !a.is_empty())
        .or_else(|| {
            e["artist"]
                .as_str()
                .or_else(|| e["uploader"].as_str())
                .or_else(|| e["channel"].as_str())
                .map(|s| vec![s.to_owned()])
        })
        .unwrap_or_default();
    Some(TrackInfo {
        title,
        artists,
        duration_ms: e["duration"].as_f64().filter(|d| *d > 0.0).map(|d| (d * 1000.0) as u64),
        thumbnail: thumb_of(e),
        url,
        origin: if is_yt { TrackOrigin::Youtube } else { TrackOrigin::Other },
        youtube_id: if is_yt { id.map(str::to_owned) } else { None },
        spotify_id: None,
        isrc: e["isrc"].as_str().map(str::to_owned),
        music: None,
    })
}

pub(crate) fn parse_probe(v: &Value) -> (Vec<TrackInfo>, Option<String>) {
    if v["_type"].as_str() == Some("playlist") {
        let tracks = v["entries"].as_array().map(|a| a.iter().filter_map(entry_to_track).collect()).unwrap_or_default();
        (tracks, v["title"].as_str().map(str::to_owned))
    } else {
        (entry_to_track(v).into_iter().collect(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reasons_are_short() {
        assert_eq!(
            short_reason("WARNING: x\nERROR: [youtube] abc: Sign in to confirm you\u{2019}re not a bot. Use --cookies"),
            "Sign in to confirm you're not a bot"
        );
        assert_eq!(short_reason("ERROR: [youtube] dQw4w9WgXcQ: Video unavailable. This video is private"), "Private video");
        assert_eq!(short_reason("ERROR: [youtube] dQw4w9WgXcQ: Video unavailable"), "Video unavailable");
        assert_eq!(short_reason("ERROR: [generic] Something odd happened here"), "Something odd happened here");
        assert_eq!(short_reason(""), "yt-dlp failed");
        assert!(short_reason(&"x".repeat(1000)).len() <= 160);
    }

    #[test]
    fn parses_youtube_opus_stream() {
        let v = json!({ "id": "dQw4w9WgXcQ",
            "url": "https://rr4---sn.googlevideo.com/videoplayback?expire=1795345435&itag=251&mime=audio%2Fwebm",
            "http_headers": { "User-Agent": "UA", "Accept": "*/*" }, "acodec": "opus", "ext": "webm",
            "format_id": "251", "protocol": "https", "duration": 213.061, "is_live": false });
        let s = parse_stream(&v, 1_000).unwrap();
        assert_eq!(s.expires_at, 1795345435);
        assert!(s.is_opus);
        assert!(!s.needs_ytdlp);
        assert_eq!(s.format_id.as_deref(), Some("251"));
        assert_eq!(s.duration_ms, Some(213_061));
        assert_eq!(s.http_headers[0].0, "Accept");
    }

    #[test]
    fn parses_hls_stream_as_ytdlp_fallback() {
        let v = json!({ "url": "https://cf.example/playlist.m3u8", "acodec": "mp4a.40.2", "ext": "mp4", "protocol": "m3u8_native" });
        let s = parse_stream(&v, 1_000).unwrap();
        assert!(!s.is_opus);
        assert!(s.needs_ytdlp);
        assert_eq!(s.expires_at, 1_000 + DEFAULT_URL_TTL);
        assert!(parse_stream(&json!({}), 0).is_none());
    }

    #[test]
    fn parses_probe_single_and_playlist() {
        let single = json!({ "_type": "video", "id": "123", "title": "Song", "uploader": "Someone",
            "duration": 200.5, "webpage_url": "https://soundcloud.com/a/b", "extractor_key": "Soundcloud",
            "thumbnails": [ { "url": "s", "width": 10, "height": 10 }, { "url": "l", "width": 500, "height": 500 } ] });
        let (t, name) = parse_probe(&single);
        assert!(name.is_none());
        assert_eq!(t[0].artists, vec!["Someone"]);
        assert_eq!(t[0].duration_ms, Some(200_500));
        assert_eq!(t[0].thumbnail.as_deref(), Some("l"));
        assert_eq!(t[0].origin, TrackOrigin::Other);

        let pl = json!({ "_type": "playlist", "title": "Set", "entries": [
            { "id": "dQw4w9WgXcQ", "ie_key": "Youtube", "title": "A", "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ" },
            { "id": "x", "title": "B", "url": "https://soundcloud.com/x" } ] });
        let (t, name) = parse_probe(&pl);
        assert_eq!(name.as_deref(), Some("Set"));
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].youtube_id.as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(t[0].origin, TrackOrigin::Youtube);
        assert!(t[1].youtube_id.is_none());
    }

    #[tokio::test]
    async fn reached_availability_is_not_waited_for() {
        let mut s = parse_stream(&json!({ "url": "https://x.invalid/v", "protocol": "https" }), 0).unwrap();
        assert_eq!(wait_until_available(&s).await, Duration::ZERO);
        s.available_at = Some(now_secs() - 1);
        assert_eq!(wait_until_available(&s).await, Duration::ZERO);
    }

    #[test]
    fn default_client_retry_rules() {
        assert!(retry_with_default_client(&SourceError::Unavailable("Requested format is not available".into())));
        assert!(retry_with_default_client(&SourceError::Unavailable("Sign in to confirm you're not a bot".into())));
        assert!(retry_with_default_client(&SourceError::Other("yt-dlp returned no stream info".into())));
        assert!(!retry_with_default_client(&SourceError::Unavailable("Private video".into())));
        assert!(!retry_with_default_client(&SourceError::Cancelled));
        assert!(!retry_with_default_client(&SourceError::Timeout));
    }

    /// A fake yt-dlp in `root/bin` that logs its arguments (one line per run) to `root/runs`,
    /// takes 300 ms, and fails runs with a player client while `root/fail_fast` exists.
    #[cfg(unix)]
    fn fake_ytdlp(client: Option<&str>) -> (Ytdlp, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("lynx-fake-ytdlp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> "{root}/runs"
sleep 0.3
case "$*" in
  *player_client=*) if [ -e "{root}/fail_fast" ]; then echo "ERROR: [youtube] x: Requested format is not available" >&2; exit 1; fi ;;
esac
echo '{{"id":"x","url":"https://x.invalid/videoplayback?expire=4102444800&itag=251","acodec":"opus","ext":"webm","format_id":"251","protocol":"https","duration":200.0,"is_live":false,"available_at":1}}'
"#,
            root = root.display()
        );
        let bin = root.join("bin/yt-dlp");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let y = Ytdlp::new(
            root.join("bin"),
            ProcessRegistry::new(),
            StreamDirs::new(root.join("streams"), 100),
            reqwest::Client::new(),
            4,
            None,
        )
        .with_cache_dir(Some(root.join("cache")))
        .with_player_client(client.map(str::to_owned));
        (y, root)
    }

    #[cfg(unix)]
    fn runs(root: &Path) -> Vec<String> {
        std::fs::read_to_string(root.join("runs")).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_resolves_of_one_video_share_a_run() {
        let (y, root) = fake_ytdlp(Some("visionos"));
        let owner = ProcOwner::system();
        let url = "https://www.youtube.com/watch?v=x";
        let (a, b, c) = tokio::join!(
            y.resolve_stream_stats(url, "x", &owner),
            y.resolve_stream_stats(url, "x", &owner),
            y.resolve_stream_stats(url, "x", &owner),
        );
        let (a, b, c) = (a.unwrap(), b.unwrap(), c.unwrap());
        assert_eq!(runs(&root).len(), 1, "one yt-dlp run for three callers");
        assert_eq!(a.0, b.0);
        assert_eq!(b.0, c.0);
        assert_eq!([a.1.cache_hit, b.1.cache_hit, c.1.cache_hit].iter().filter(|h| !**h).count(), 1);
        assert!(a.0.is_opus);
        assert!(y.inner.inflight.lock().unwrap().is_empty(), "no in-flight entry left behind");

        // A warm-up and the player's resolve: still one run, and the player takes the cached URL.
        let (_, d) = tokio::join!(y.warm(url, "y", &owner), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            y.resolve_stream_stats(url, "y", &owner).await
        });
        assert!(d.unwrap().1.cache_hit);
        assert_eq!(runs(&root).len(), 2);
        // Different keys run side by side.
        let start = std::time::Instant::now();
        let (e, f) = tokio::join!(y.resolve_stream(url, "e", &owner), y.resolve_stream(url, "f", &owner));
        e.unwrap();
        f.unwrap();
        assert_eq!(runs(&root).len(), 4);
        assert!(start.elapsed() < Duration::from_millis(550), "independent keys are not serialised");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fast_client_first_then_default_clients() {
        let (y, root) = fake_ytdlp(Some("visionos"));
        let owner = ProcOwner::system();
        let url = "https://www.youtube.com/watch?v=x";
        y.resolve_stream(url, "a", &owner).await.unwrap();
        let r = runs(&root);
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("youtube:player_client=visionos"), "{}", r[0]);
        assert!(r[0].contains(&format!("--cache-dir {}", root.join("cache").display())), "{}", r[0]);

        // The fast client fails: the same resolve retries with yt-dlp's default clients.
        std::fs::write(root.join("fail_fast"), "").unwrap();
        y.resolve_stream(url, "b", &owner).await.unwrap();
        let r = runs(&root);
        assert_eq!(r.len(), 3);
        assert!(r[1].contains("player_client=visionos"));
        assert!(!r[2].contains("player_client"), "{}", r[2]);

        // A refused URL is resolved again with the default clients only.
        std::fs::remove_file(root.join("fail_fast")).unwrap();
        y.invalidate("a");
        y.resolve_stream(url, "a", &owner).await.unwrap();
        let r = runs(&root);
        assert_eq!(r.len(), 4);
        assert!(!r[3].contains("player_client"), "{}", r[3]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_player_client_means_yt_dlp_default() {
        for client in [None, Some(""), Some("default")] {
            let (y, root) = fake_ytdlp(client);
            y.resolve_stream("https://www.youtube.com/watch?v=x", "x", &ProcOwner::system()).await.unwrap();
            let r = runs(&root);
            assert_eq!(r.len(), 1);
            assert!(!r[0].contains("player_client"), "{client:?}: {}", r[0]);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    // ---- network (cargo test -- --ignored ytdlp_)

    fn ytdlp() -> (Ytdlp, PathBuf) {
        let root = std::env::temp_dir().join(format!("lynx-ytdlp-{}", uuid::Uuid::new_v4()));
        let y = Ytdlp::new(
            root.join("bin"),
            ProcessRegistry::new(),
            StreamDirs::new(root.join("streams"), 100),
            reqwest::Client::new(),
            2,
            None,
        );
        (y, root)
    }

    #[tokio::test]
    #[ignore]
    async fn ytdlp_resolve_youtube_opus() {
        let (y, root) = ytdlp();
        let owner = ProcOwner::new(1, 2, Some(uuid::Uuid::new_v4()));
        let s = y.resolve_stream("https://www.youtube.com/watch?v=dQw4w9WgXcQ", "dQw4w9WgXcQ", &owner).await.unwrap();
        println!("format={:?} codec={:?} container={:?} opus={} expires_in={}s", s.format_id, s.codec, s.container, s.is_opus, s.expires_at - now_secs());
        assert!(s.url.starts_with("https://"));
        assert!(s.expires_at > now_secs() + 3600);
        assert!(!s.needs_ytdlp);
        // Cached: second call does not spawn.
        let again = y.resolve_stream("https://www.youtube.com/watch?v=dQw4w9WgXcQ", "dQw4w9WgXcQ", &owner).await.unwrap();
        assert_eq!(again, s);
        // The track dir was used as work dir and still exists for the player to remove.
        assert!(root.join("streams/1/2").join(owner.track_uid.unwrap().to_string()).is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    #[ignore]
    async fn ytdlp_unavailable_reason() {
        let (y, root) = ytdlp();
        let err = y.resolve_stream("https://www.youtube.com/watch?v=aaaaaaaaaaa", "aaaaaaaaaaa", &ProcOwner::system()).await.unwrap_err();
        println!("{err}");
        assert!(matches!(err, SourceError::Unavailable(_)), "{err:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    #[ignore]
    async fn ytdlp_probe_other_url() {
        let (y, root) = ytdlp();
        let (t, _) = y.probe("https://soundcloud.com/forss/flickermood", 10, &ProcOwner::system()).await.unwrap();
        println!("{t:#?}");
        assert_eq!(t.len(), 1);
        assert!(t[0].duration_ms.is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    #[ignore]
    async fn ytdlp_self_update() {
        use std::os::unix::fs::PermissionsExt;
        let (y, root) = ytdlp();
        // Pretend bin/ holds an outdated copy so the download path runs.
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let fake = root.join("bin/yt-dlp");
        std::fs::write(&fake, "#!/bin/sh\necho 2000.01.01\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(y.version().await.as_deref(), Some("2000.01.01"));
        let r = y.update().await.unwrap();
        println!("update result: {r:?}, binary: {}, version now {:?}", y.binary().display(), y.version().await);
        assert!(r.is_some());
        assert_eq!(y.version().await, r);
        // No temp files left behind.
        assert_eq!(std::fs::read_dir(root.join("bin")).unwrap().count(), 1);
        assert!(y.update().await.unwrap().is_none(), "second update must be a no-op");
        let _ = std::fs::remove_dir_all(root);
    }
}
