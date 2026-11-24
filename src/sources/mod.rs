//! Turning user input (URLs, search text, Spotify links) into playable tracks.
//!
//! [`Sources`] is the only entry point the rest of the app needs: `resolve` (input -> tracks),
//! `prepare` (track -> direct stream URL, doing the lazy Spotify match), `search` and `lyrics`.
//! It knows nothing about Discord or songbird.

#![allow(dead_code)]

pub mod apple;
pub mod counterpart;
pub mod detect;
pub mod fallback;
pub mod lists;
pub mod lyrics;
pub mod matcher;
pub mod spotify;
pub mod startup;
pub mod text;
pub mod youtube;
pub mod ytdlp;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::{
    config::Config,
    procs::{ProcOwner, ProcessRegistry, StreamDirs},
    store::Store,
};
use detect::Input;
pub use lists::{CachedList, ListRef};
pub use lyrics::Lyrics;
pub use startup::{LateMusic, MetaTiming, PrepareTimings};
use startup::{MetaLookup, MetaOutcome};
use ytdlp::ResolveStats;
pub use ytdlp::StreamInfo;

/// Generic client identification for third-party APIs. Never put personal data here.
pub const USER_AGENT: &str = "Lynx (https://github.com/pixlgalaxy)";
/// Default cap for playlists, albums and yt-dlp sets.
pub const DEFAULT_LIST_LIMIT: usize = 1000;
const SEARCH_CACHE_NS: &str = "search";
/// Parallel YouTube Music counterpart lookups (InnerTube is cheap, but be polite).
const MUSIC_LOOKUPS: usize = 4;
/// A slow counterpart lookup never holds up playback longer than this.
const MUSIC_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Once the stream is ready, how much longer `prepare` waits for a metadata lookup still running
/// (so a quick one makes the first now-playing card). Past that the track starts without it.
const MUSIC_GRACE: Duration = Duration::from_millis(250);
/// In-memory original -> substitute ids kept for stream invalidation.
const SUBSTITUTES_MAX: usize = 2048;

/// Where a track was originally requested from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackOrigin {
    Youtube,
    YoutubeMusic,
    Spotify,
    AppleMusic,
    Other,
}

impl TrackOrigin {
    /// Song catalogs whose metadata is clean and that need a YouTube match before playing.
    pub fn is_catalog(self) -> bool {
        matches!(self, Self::Spotify | Self::AppleMusic)
    }
}

/// Metadata for one queue entry. Spotify entries may not have a YouTube match yet (`youtube_id == None`):
/// they are resolved lazily shortly before they play.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackInfo {
    pub title: String,
    pub artists: Vec<String>,
    pub duration_ms: Option<u64>,
    pub thumbnail: Option<String>,
    /// Canonical page URL shown to users.
    pub url: String,
    pub origin: TrackOrigin,
    pub youtube_id: Option<String>,
    pub spotify_id: Option<String>,
    pub isrc: Option<String>,
    /// Clean YouTube Music metadata for this recording (the "Artist - Topic" audio track that
    /// matches a pasted music video, or the song itself). Playback still uses `youtube_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub music: Option<MusicMeta>,
}

/// YouTube Music song entry (`MUSIC_VIDEO_TYPE_ATV`) a track corresponds to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MusicMeta {
    /// Video id of the audio track (equal to `TrackInfo::youtube_id` when the track is the song).
    pub youtube_id: String,
    pub title: String,
    pub artists: Vec<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub isrc: Option<String>,
}

impl TrackInfo {
    /// Title to show and index: the clean song title when known.
    pub fn display_title(&self) -> &str {
        self.music
            .as_ref()
            .map(|m| m.title.as_str())
            .filter(|t| !t.is_empty())
            .unwrap_or(&self.title)
    }

    /// Artists to show and index: the song's artist list when known.
    pub fn display_artists(&self) -> &[String] {
        match &self.music {
            Some(m) if !m.artists.is_empty() => &m.artists,
            _ => &self.artists,
        }
    }

    /// ISRC of the track or of its YouTube Music song.
    pub fn any_isrc(&self) -> Option<&str> {
        self.isrc
            .as_deref()
            .or_else(|| self.music.as_ref().and_then(|m| m.isrc.as_deref()))
            .filter(|s| !s.is_empty())
    }

    /// The track is a YouTube Music song itself (not a video with a song counterpart).
    pub fn is_music_song(&self) -> bool {
        self.music
            .as_ref()
            .is_some_and(|m| self.youtube_id.as_deref() == Some(m.youtube_id.as_str()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// Nothing found for the input (search without results, 404 playlist...).
    #[error("{0}")]
    NotFound(String),
    /// The item exists but cannot be played; carries a short reason (e.g. from yt-dlp stderr).
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("Spotify links are not enabled on this bot")]
    SpotifyDisabled,
    #[error("timed out")]
    Timeout,
    /// The process was killed because its track or session ended.
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Http(String),
    #[error("{0}")]
    Other(String),
}

impl From<reqwest::Error> for SourceError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Self::Timeout
        } else if e.is_decode() {
            Self::Http("unexpected response from the service".into())
        } else {
            // No URLs in the message: they may carry tokens or user input.
            Self::Http(e.without_url().to_string())
        }
    }
}

pub type SourceResult<T> = Result<T, SourceError>;

#[derive(Debug, Clone)]
pub struct Resolved {
    pub tracks: Vec<TrackInfo>,
    pub playlist_name: Option<String>,
    /// Cover of the list, when the platform gives one.
    pub image: Option<String>,
}

/// Everything the player needs to start a track.
#[derive(Debug, Clone)]
pub struct Playable {
    /// The track, with `youtube_id` filled in for matched Spotify entries.
    pub track: TrackInfo,
    pub stream: StreamInfo,
    /// The stream is fragmented (HLS/DASH) or live: play it by piping `page_url` through yt-dlp
    /// (in `work_dir`) instead of reading `stream.url` directly.
    pub ytdlp_fallback: bool,
    /// The URL yt-dlp resolved (YouTube watch URL or the original page).
    pub page_url: String,
    /// Per-track temp dir (`streams/{guild}/{channel}/{track}`), when the owner has a track.
    pub work_dir: Option<PathBuf>,
    /// The video that actually plays. Differs from `track.youtube_id` when the original was
    /// unavailable and an alternative upload of the same recording is played instead
    /// (`track` keeps the original id for history and the catalog). `None` for non-YouTube URLs.
    pub played_youtube_id: Option<String>,
    /// Why an alternative plays, for logging.
    pub fallback_reason: Option<String>,
    /// YouTube Music metadata still being looked up when the stream was ready (`track.music`
    /// is filled in by the player when it arrives).
    pub late_music: Option<LateMusic>,
    pub timings: PrepareTimings,
}

#[derive(Clone)]
pub struct Sources {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    ttl: Duration,
    http: reqwest::Client,
    dirs: StreamDirs,
    yt: youtube::YouTube,
    ytdlp: ytdlp::Ytdlp,
    spotify: Option<spotify::Spotify>,
    apple: apple::Apple,
    lrclib: lyrics::Lrclib,
    music_lookups: Semaphore,
    /// Shared track lists of playlists / albums (60 min, every user and guild).
    lists: lists::ListCache,
    /// Original video id -> substitute that played (see `invalidate_stream`).
    substitutes: Mutex<HashMap<String, String>>,
}

impl Sources {
    pub fn new(config: &Config, store: Store, procs: ProcessRegistry) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()?;
        let dirs = StreamDirs::new(config.streams_dir(), config.stream_cache_max_mb);
        let spotify = match (&config.spotify_client_id, &config.spotify_client_secret) {
            (Some(id), Some(secret)) => Some(spotify::Spotify::new(http.clone(), id, secret)),
            _ => None,
        };
        Ok(Self {
            inner: Arc::new(Inner {
                store: store.clone(),
                ttl: Duration::from_secs(u64::from(config.cache_ttl_days.max(1)) * 24 * 60 * 60),
                yt: youtube::YouTube::new(http.clone()),
                ytdlp: ytdlp::Ytdlp::new(
                    config.bin_dir(),
                    procs,
                    dirs.clone(),
                    http.clone(),
                    config.max_concurrent_extractions,
                    config.ytdlp_cookies_file.clone(),
                )
                .with_limit_rate(config.ytdlp_limit_rate.clone())
                .with_cache_dir(Some(config.ytdlp_cache_dir()))
                .with_player_client(config.ytdlp_player_client.clone()),
                spotify,
                apple: apple::Apple::new(http.clone()),
                lrclib: lyrics::Lrclib::new(http.clone()),
                music_lookups: Semaphore::new(MUSIC_LOOKUPS),
                lists: lists::ListCache::new(store.clone(), lists::LIST_TTL),
                substitutes: Mutex::new(HashMap::new()),
                http,
                dirs,
            }),
        })
    }

    pub fn stream_dirs(&self) -> &StreamDirs {
        &self.inner.dirs
    }

    pub fn ytdlp(&self) -> &ytdlp::Ytdlp {
        &self.inner.ytdlp
    }

    /// yt-dlp self-update at startup and every 24 h. Spawn it; it ends on `stop`.
    pub fn update_task(&self, stop: CancellationToken) -> impl std::future::Future<Output = ()> + Send + 'static {
        self.inner.ytdlp.update_task(stop)
    }

    /// Turn user input into tracks. Spotify tracks come back unmatched (`youtube_id: None`).
    /// `owner` is used for any yt-dlp process this needs (non-YouTube URLs).
    pub async fn resolve(&self, query: &str, limit: usize, owner: &ProcOwner) -> SourceResult<Resolved> {
        let input = self.detect(query).await?;
        self.resolve_input(input, limit, owner).await
    }

    /// Classify input, following Spotify short links.
    async fn detect(&self, query: &str) -> SourceResult<Input> {
        match detect::detect(query) {
            Input::SpotifyShortLink(url) => self.expand_short_link(&url).await,
            other => Ok(other),
        }
    }

    /// The canonical list (playlist, album, artist page) behind user input, if it is one.
    pub async fn list_ref(&self, query: &str) -> SourceResult<Option<ListRef>> {
        Ok(lists::list_ref(&self.detect(query).await?))
    }

    /// [`resolve`](Self::resolve) for `/play`: lists come from the shared list cache (fetched when
    /// stale) and are returned with their canonical reference, for saving.
    pub async fn resolve_for_play(
        &self,
        query: &str,
        limit: usize,
        owner: &ProcOwner,
    ) -> SourceResult<(Resolved, Option<ListRef>)> {
        let input = self.detect(query).await?;
        let Some(list) = lists::list_ref(&input) else {
            return Ok((self.resolve_input(input, limit, owner).await?, None));
        };
        let c = self.list(&list, false, owner).await?;
        let mut tracks = c.tracks;
        tracks.truncate(limit.max(1));
        Ok((
            Resolved {
                tracks,
                playlist_name: c.name,
                image: c.image,
            },
            Some(list),
        ))
    }

    /// Tracks of a list: the shared cache when fresh, else (or with `force`) a new fetch that
    /// replaces it. Fetches always read up to [`DEFAULT_LIST_LIMIT`] tracks, so one cache entry
    /// serves every guild's queue limit.
    pub async fn list(&self, list: &ListRef, force: bool, owner: &ProcOwner) -> SourceResult<CachedList> {
        self.inner
            .lists
            .get_or_fetch(&list.url, force, || async {
                let input = detect::detect(&list.url);
                // Apple collections have their own long-lived cache: a fetch here must be fresh.
                let apple_key = match &input {
                    Input::AppleAlbum { country, id } => Some(apple::album_key(country, id)),
                    Input::ApplePlaylist { country, id } => Some(apple::playlist_key(country, id)),
                    _ => None,
                };
                if let Some(key) = apple_key {
                    let _ = self.inner.store.cache_delete(apple::CACHE_NS, &key).await;
                }
                let r = self.resolve_input(input, DEFAULT_LIST_LIMIT, owner).await?;
                if r.tracks.is_empty() {
                    return Err(SourceError::NotFound("That list is empty".into()));
                }
                let image = r.image.or_else(|| r.tracks.iter().find_map(|t| t.thumbnail.clone()));
                Ok(CachedList {
                    name: r.playlist_name,
                    image,
                    tracks: r.tracks,
                    fetched_at: crate::store::now_secs(),
                })
            })
            .await
    }

    /// The cached tracks of a list if still fresh; never fetches.
    pub async fn cached_list(&self, list: &ListRef) -> Option<CachedList> {
        self.inner.lists.peek(&list.url).await
    }

    async fn resolve_input(&self, input: Input, limit: usize, owner: &ProcOwner) -> SourceResult<Resolved> {
        let limit = limit.max(1);
        let single = |t: TrackInfo| Resolved {
            tracks: vec![t],
            playlist_name: None,
            image: None,
        };
        match input {
            Input::Search(q) if q.is_empty() => Err(SourceError::NotFound("Empty search".into())),
            Input::Search(q) => {
                let songs = self.inner.yt.search_songs(&q, 1).await.unwrap_or_default();
                let found = match songs.into_iter().next() {
                    Some(t) => Some(t),
                    None => self.inner.yt.search_videos(&q, 1).await?.into_iter().next(),
                };
                found.map(single).ok_or_else(|| SourceError::NotFound(format!("No results for \"{q}\"")))
            }
            Input::YoutubeVideo { id, music } => match self.inner.yt.video(&id, music).await {
                Ok(t) => Ok(single(self.with_music_meta(t).await)),
                // InnerTube hiccup: let yt-dlp read the metadata instead.
                Err(SourceError::Http(_)) => {
                    let url = format!("https://www.youtube.com/watch?v={id}");
                    let (tracks, _) = self.inner.ytdlp.probe(&url, 1, owner).await?;
                    Ok(Resolved { tracks, playlist_name: None, image: None })
                }
                Err(e) => Err(e),
            },
            Input::YoutubePlaylist { id, music } => {
                let p = self.inner.yt.playlist(&id, music, limit).await?;
                Ok(Resolved { tracks: p.tracks, playlist_name: p.title, image: None })
            }
            Input::YoutubeMusicAlbum(id) => {
                let p = self.inner.yt.music_album(&id, limit).await?;
                Ok(Resolved { tracks: p.tracks, playlist_name: p.title, image: None })
            }
            Input::YoutubeMusicArtist(id) => {
                let p = self.inner.yt.music_artist(&id, limit).await?;
                Ok(Resolved { tracks: p.tracks, playlist_name: p.title, image: None })
            }
            Input::SpotifyTrack(id) => Ok(single(self.spotify()?.track(&id).await?)),
            Input::SpotifyAlbum(id) => {
                let c = self.spotify()?.album(&id, limit).await?;
                Ok(Resolved { tracks: c.tracks, playlist_name: c.name, image: c.image })
            }
            Input::SpotifyPlaylist(id) => {
                let c = self.spotify()?.playlist(&id, limit).await?;
                if c.tracks.is_empty() {
                    return Err(SourceError::NotFound("That Spotify playlist is empty".into()));
                }
                Ok(Resolved { tracks: c.tracks, playlist_name: c.name, image: c.image })
            }
            Input::AppleSong { country, id } => {
                Ok(single(self.inner.apple.song(&self.inner.store, self.inner.ttl, &country, &id).await?))
            }
            Input::AppleAlbum { country, id } => {
                let c = self.inner.apple.album(&self.inner.store, self.inner.ttl, &country, &id, limit).await?;
                Ok(Resolved { tracks: c.tracks, playlist_name: c.name, image: None })
            }
            Input::ApplePlaylist { country, id } => {
                let c = self.inner.apple.playlist(&self.inner.store, self.inner.ttl, &country, &id, limit).await?;
                Ok(Resolved { tracks: c.tracks, playlist_name: c.name, image: None })
            }
            Input::AppleUnsupported(_) => Err(SourceError::Unsupported(
                "Only Apple Music songs, albums and playlists can be played".into(),
            )),
            Input::SpotifyUnsupported(_) | Input::SpotifyShortLink(_) => Err(SourceError::Unsupported(
                "Only Spotify tracks, albums and playlists can be played".into(),
            )),
            Input::Url(url) => {
                let (tracks, name) = self.inner.ytdlp.probe(&url, limit, owner).await?;
                Ok(Resolved { tracks, playlist_name: name, image: None })
            }
        }
    }

    fn spotify(&self) -> SourceResult<&spotify::Spotify> {
        self.inner.spotify.as_ref().ok_or(SourceError::SpotifyDisabled)
    }

    /// spotify.link / spoti.fi redirect to open.spotify.com.
    async fn expand_short_link(&self, url: &str) -> SourceResult<Input> {
        let res = self
            .inner
            .http
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        let final_url = res.url().to_string();
        match detect::detect(&final_url) {
            i @ (Input::SpotifyTrack(_) | Input::SpotifyAlbum(_) | Input::SpotifyPlaylist(_)) => Ok(i),
            _ => Err(SourceError::Unsupported("Could not open that Spotify link".into())),
        }
    }

    /// Spotify / Apple Music -> YouTube match (cached). No-op for tracks that already have a video id.
    pub async fn ensure_youtube(&self, track: &TrackInfo) -> SourceResult<TrackInfo> {
        if track.youtube_id.is_some() || !track.origin.is_catalog() {
            return Ok(track.clone());
        }
        matcher::find(&self.inner.yt, &self.inner.store, self.inner.ttl, track).await
    }

    /// Attach the YouTube Music song metadata of a YouTube track (cached, negative results too).
    /// Never fails: on any error the track comes back unchanged.
    pub async fn with_music_meta(&self, mut track: TrackInfo) -> TrackInfo {
        if needs_music_meta(&track) {
            track.music = self.music_meta(&track).await;
        }
        track
    }

    async fn music_meta(&self, track: &TrackInfo) -> Option<MusicMeta> {
        let inner = &self.inner;
        let lookup = async {
            let _permit = inner.music_lookups.acquire().await.ok()?;
            match counterpart::lookup(&inner.yt, &inner.store, inner.ttl, track).await {
                Ok(m) => m,
                Err(e) => {
                    debug!(error = %e, id = ?track.youtube_id, "youtube music lookup failed");
                    None
                }
            }
        };
        tokio::time::timeout(MUSIC_LOOKUP_TIMEOUT, lookup).await.ok().flatten()
    }

    /// Everything needed to start playing `track` now: lazy Spotify match, YouTube Music metadata
    /// (for playlist entries, looked up only when they are about to play), stream URL and, when
    /// the video is unavailable here, an alternative upload of the same recording.
    ///
    /// The metadata never decides which video is resolved, so it is looked up alongside the
    /// stream; if it is still running when the stream is ready (plus [`MUSIC_GRACE`]) it comes
    /// back as [`Playable::late_music`] instead of holding up the start.
    pub async fn prepare(&self, track: &TrackInfo, owner: &ProcOwner) -> SourceResult<Playable> {
        let mut t = PrepareTimings::new(tokio::time::Instant::now());
        let matching = track.youtube_id.is_none() && track.origin.is_catalog();
        let track = self.ensure_youtube(track).await?;
        t.matched = matching.then(|| t.requested.elapsed());
        let mut meta = needs_music_meta(&track).then(|| {
            let (sources, track) = (self.clone(), track.clone());
            MetaLookup::spawn(async move { sources.music_meta(&track).await })
        });
        let resolving = tokio::time::Instant::now();
        let mut playable = self.prepare_stream(track, &mut meta, &mut t, owner).await?;
        t.resolve = resolving.elapsed().saturating_sub(t.waited);
        if let Some(lookup) = meta {
            match lookup.settle(MUSIC_GRACE).await {
                MetaOutcome::Ready(music, took) => {
                    t.meta = MetaTiming::Ready(took);
                    if music.is_some() {
                        playable.track.music = music;
                    }
                }
                MetaOutcome::Late(late) => {
                    t.meta = MetaTiming::Late;
                    playable.late_music = Some(late);
                }
            }
        }
        t.prepared = t.requested.elapsed();
        debug!(
            id = ?playable.track.youtube_id,
            matched_ms = ?t.matched.map(|d| d.as_millis()),
            meta = ?t.meta,
            resolve_ms = t.resolve.as_millis() as u64,
            cache_hit = t.cache_hit,
            wait_ms = t.waited.as_millis() as u64,
            prepare_ms = t.prepared.as_millis() as u64,
            "track prepared"
        );
        playable.timings = t;
        Ok(playable)
    }

    /// The stream part of [`Self::prepare`]. A metadata lookup still in `meta` is only awaited
    /// before looking for alternatives (the YouTube Music song is one of them).
    async fn prepare_stream(
        &self,
        mut track: TrackInfo,
        meta: &mut Option<MetaLookup>,
        t: &mut PrepareTimings,
        owner: &ProcOwner,
    ) -> SourceResult<Playable> {
        let Some(orig) = track.youtube_id.clone() else {
            let (stream, stats) = self.inner.ytdlp.resolve_stream_stats(&track.url, &track.url, owner).await?;
            record(t, stats);
            let page_url = track.url.clone();
            return Ok(self.playable(track, stream, page_url, None, None, owner));
        };
        let store = &self.inner.store;
        let cached: Option<Option<String>> = store.cache_get(fallback::CACHE_NS, &orig).await.ok().flatten();
        let mut tried = HashSet::from([orig.clone()]);
        if let Some(Some(alt)) = &cached {
            match self.youtube_stream(alt, owner).await {
                Ok((stream, stats)) => {
                    record(t, stats);
                    t.fallback = true;
                    let reason = format!("{orig} is unavailable here; playing the known substitute {alt}");
                    return Ok(self.substitute(track, &orig, alt, stream, reason, owner));
                }
                Err(SourceError::Cancelled) => return Err(SourceError::Cancelled),
                Err(_) => {
                    tried.insert(alt.clone());
                }
            }
        }
        let err = match self.youtube_stream(&orig, owner).await {
            Ok((stream, stats)) => {
                record(t, stats);
                return Ok(self.playable(track, stream, watch_url(&orig), Some(orig), None, owner));
            }
            // "No alternative" is cached: fail right away.
            Err(e) if fallback::fallback_worthy(&e) && cached != Some(None) => e,
            Err(e) => return Err(e),
        };
        // The alternatives (and which steps apply) depend on the song metadata: wait for it now.
        if let Some(lookup) = meta.take() {
            let (music, took) = lookup.wait().await;
            t.meta = MetaTiming::Ready(took);
            if music.is_some() {
                track.music = music;
            }
        }
        let alts = LiveAlternatives {
            sources: self,
            track: &track,
            owner,
            last: Mutex::new(ResolveStats::default()),
        };
        match fallback::try_alternatives(&alts, fallback::steps_for(&track), tried, fallback::MAX_ALTERNATIVES).await {
            fallback::Outcome::Played(f) => {
                let stats = *alts.last.lock().unwrap();
                record(t, stats);
                t.fallback = true;
                let _ = store.cache_put(fallback::CACHE_NS, &orig, &Some(f.id.clone()), self.inner.ttl).await;
                let reason = format!("{orig} is unavailable ({err}); playing {} ({})", f.id, f.step.label());
                Ok(self.substitute(track, &orig, &f.id, f.stream, reason, owner))
            }
            fallback::Outcome::NoAlternative => {
                let _ = store.cache_put(fallback::CACHE_NS, &orig, &None::<String>, self.inner.ttl).await;
                Err(err)
            }
            fallback::Outcome::Inconclusive => Err(err),
        }
    }

    async fn youtube_stream(&self, id: &str, owner: &ProcOwner) -> SourceResult<(StreamInfo, ResolveStats)> {
        self.inner.ytdlp.resolve_stream_stats(&watch_url(id), id, owner).await
    }

    fn substitute(
        &self,
        track: TrackInfo,
        orig: &str,
        alt: &str,
        stream: StreamInfo,
        reason: String,
        owner: &ProcOwner,
    ) -> Playable {
        debug!(%reason, "playback fallback");
        {
            let mut subs = self.inner.substitutes.lock().unwrap();
            if subs.len() >= SUBSTITUTES_MAX {
                subs.clear();
            }
            subs.insert(orig.to_owned(), alt.to_owned());
        }
        self.playable(track, stream, watch_url(alt), Some(alt.to_owned()), Some(reason), owner)
    }

    fn playable(
        &self,
        track: TrackInfo,
        stream: StreamInfo,
        page_url: String,
        played_youtube_id: Option<String>,
        fallback_reason: Option<String>,
        owner: &ProcOwner,
    ) -> Playable {
        let work_dir = owner.track_uid.map(|t| self.inner.dirs.track_path(owner.guild_id, owner.voice_channel_id, t));
        Playable {
            ytdlp_fallback: stream.needs_ytdlp,
            track,
            stream,
            page_url,
            work_dir,
            played_youtube_id,
            fallback_reason,
            late_music: None,
            timings: PrepareTimings::new(tokio::time::Instant::now()),
        }
    }

    /// Start resolving the stream of a track about to play (e.g. while the bot joins the voice
    /// channel), so [`Self::prepare`] finds the URL cached or joins this run. Runs in the
    /// background; never fails. Catalog tracks are matched first (the match is cached too).
    pub fn warm_stream(&self, track: &TrackInfo, owner: &ProcOwner) {
        let (sources, track, owner) = (self.clone(), track.clone(), owner.clone());
        tokio::spawn(async move {
            let Ok(track) = sources.ensure_youtube(&track).await else {
                return;
            };
            match &track.youtube_id {
                Some(id) => {
                    // A known substitute (or known failure) is handled by prepare itself.
                    let known: Option<Option<String>> =
                        sources.inner.store.cache_get(fallback::CACHE_NS, id).await.ok().flatten();
                    if known.is_none() {
                        sources.inner.ytdlp.warm(&watch_url(id), id, &owner).await;
                    }
                }
                None => sources.inner.ytdlp.warm(&track.url, &track.url, &owner).await,
            }
        });
    }

    /// Forget a cached stream URL (e.g. after the CDN answered 403), including the substitute's.
    pub fn invalidate_stream(&self, track: &TrackInfo) {
        let key = track.youtube_id.as_deref().unwrap_or(&track.url);
        self.inner.ytdlp.invalidate(key);
        let alt = self.inner.substitutes.lock().unwrap().get(key).cloned();
        if let Some(alt) = alt {
            self.inner.ytdlp.invalidate(&alt);
        }
    }

    /// Search for the panel / autocomplete: YouTube Music songs first, then regular videos.
    pub async fn search(&self, query: &str, limit: usize) -> SourceResult<Vec<TrackInfo>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 25);
        let key = format!("{}|{limit}", text::normalize(q));
        if let Ok(Some(hit)) = self.inner.store.cache_get::<Vec<TrackInfo>>(SEARCH_CACHE_NS, &key).await {
            return Ok(hit);
        }
        let (songs, videos) =
            tokio::join!(self.inner.yt.search_songs(q, limit), self.inner.yt.search_videos(q, limit));
        if let (Err(e), Err(_)) = (&songs, &videos) {
            return Err(SourceError::Http(e.to_string()));
        }
        let mut out = songs.unwrap_or_default();
        let mut seen: std::collections::HashSet<String> = out.iter().filter_map(|t| t.youtube_id.clone()).collect();
        for v in videos.unwrap_or_default() {
            if out.len() >= limit {
                break;
            }
            if v.youtube_id.as_ref().is_some_and(|id| seen.insert(id.clone())) {
                out.push(v);
            }
        }
        out.truncate(limit);
        if !out.is_empty() {
            let _ = self.inner.store.cache_put(SEARCH_CACHE_NS, &key, &out, self.inner.ttl).await;
        }
        Ok(out)
    }

    pub async fn lyrics(&self, track: &TrackInfo) -> SourceResult<Option<Lyrics>> {
        self.inner.lrclib.lyrics(&self.inner.store, self.inner.ttl, track).await
    }
}

fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// A YouTube track whose YouTube Music song metadata is not known yet.
fn needs_music_meta(track: &TrackInfo) -> bool {
    let youtube = matches!(track.origin, TrackOrigin::Youtube | TrackOrigin::YoutubeMusic);
    track.music.is_none() && youtube && track.youtube_id.is_some()
}

/// Keep the stats of the resolve that produced the playing stream.
fn record(t: &mut PrepareTimings, stats: ResolveStats) {
    t.cache_hit = stats.cache_hit;
    t.waited = stats.waited;
}

/// The real alternative sources for one track.
struct LiveAlternatives<'a> {
    sources: &'a Sources,
    track: &'a TrackInfo,
    owner: &'a ProcOwner,
    /// Stats of the last stream tried (the one that played, when one did).
    last: Mutex<ResolveStats>,
}

impl fallback::Alternatives for LiveAlternatives<'_> {
    async fn candidates(&self, step: fallback::Step) -> SourceResult<Vec<String>> {
        let orig = self.track.youtube_id.as_deref();
        match step {
            fallback::Step::MusicSong => {
                let meta = match &self.track.music {
                    Some(m) => Some(m.clone()),
                    // The lookup in `prepare` may have timed out: one more (cached) try.
                    None => self.sources.music_meta(self.track).await,
                };
                Ok(meta
                    .map(|m| m.youtube_id)
                    .filter(|id| Some(id.as_str()) != orig)
                    .into_iter()
                    .collect())
            }
            fallback::Step::Video => {
                let readings = counterpart::clean_readings(self.track);
                let Some(first) = readings.first() else {
                    return Ok(Vec::new());
                };
                let videos = self
                    .sources
                    .inner
                    .yt
                    .search_videos(&counterpart::video_query(first), 10)
                    .await?;
                Ok(counterpart::confident_videos(&readings, &videos))
            }
        }
    }

    async fn try_stream(&self, id: &str) -> SourceResult<StreamInfo> {
        let (stream, stats) = self.sources.youtube_stream(id, self.owner).await?;
        *self.last.lock().unwrap() = stats;
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_track_json_without_music_still_loads() {
        let old = r#"{"title":"Despacito","artists":["Luis Fonsi"],"duration_ms":282000,"thumbnail":null,
            "url":"https://www.youtube.com/watch?v=kJQP7kiw5Fk","origin":"youtube","youtube_id":"kJQP7kiw5Fk",
            "spotify_id":null,"isrc":null}"#;
        let t: TrackInfo = serde_json::from_str(old).unwrap();
        assert!(t.music.is_none());
        assert_eq!(t.display_title(), "Despacito");
        // Without music meta the JSON stays the same shape.
        assert!(!serde_json::to_string(&t).unwrap().contains("music"));
    }

    #[test]
    fn music_meta_round_trip_and_display() {
        let mut t = crate::store::test_util::track("kJQP7kiw5Fk");
        t.title = "Luis Fonsi - Despacito ft. Daddy Yankee".into();
        t.artists = vec!["LuisFonsiVEVO".into()];
        t.music = Some(MusicMeta {
            youtube_id: "atv00000000".into(),
            title: "Despacito".into(),
            artists: vec!["Luis Fonsi".into(), "Daddy Yankee".into()],
            album: Some("VIDA".into()),
            isrc: Some("USUM71703861".into()),
        });
        let back: TrackInfo = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back.music, t.music);
        assert_eq!(back.display_title(), "Despacito");
        assert_eq!(back.display_artists(), ["Luis Fonsi", "Daddy Yankee"]);
        assert_eq!(back.any_isrc(), Some("USUM71703861"));
        assert!(!back.is_music_song());
        // Playback identity is untouched.
        assert_eq!(back.youtube_id.as_deref(), Some("kJQP7kiw5Fk"));

        // The video and its song share one catalog key.
        let mut song = crate::store::test_util::track("atv00000000");
        song.music = Some(MusicMeta {
            youtube_id: "atv00000000".into(),
            ..t.music.clone().unwrap()
        });
        assert!(song.is_music_song());
        let key = crate::store::track_key;
        assert_eq!(key(&t), key(&song));
        // Partial meta (old cache entries) deserializes.
        let m: MusicMeta = serde_json::from_str(r#"{"youtube_id":"a","title":"b","artists":[]}"#).unwrap();
        assert!(m.album.is_none() && m.isrc.is_none());
    }

    fn config(data_dir: PathBuf) -> Config {
        Config {
            discord_token: String::new(),
            discord_client_id: String::new(),
            discord_client_secret: String::new(),
            node_tokens: Vec::new(),
            public_url: String::new(),
            http_host: "127.0.0.1".into(),
            http_port: 0,
            appdata_dir: data_dir.clone(),
            data_dir,
            bot_locale: "en".into(),
            spotify_client_id: std::env::var("SPOTIFY_CLIENT_ID").ok().filter(|s| !s.is_empty()),
            spotify_client_secret: std::env::var("SPOTIFY_CLIENT_SECRET").ok().filter(|s| !s.is_empty()),
            lastfm_api_key: None,
            musicbrainz_user_agent: None,
            ai_enabled: false,
            ollama_url: None,
            ollama_model: None,
            ollama_keep_alive: String::new(),
            ollama_timeout_secs: 0,
            ollama_checked_models: Vec::new(),
            ollama_load_retry_secs: 30,
            ytdlp_cookies_file: None,
            ytdlp_limit_rate: None,
            ytdlp_player_client: None,
            empty_channel_timeout_secs: 0,
            max_concurrent_extractions: 2,
            stream_cache_max_mb: 100,
            prefetch_tracks: 2,
            cache_ttl_days: 7,
            max_stored_playlists: 20,
            landing_url: None,
            terms_url: None,
            privacy_url: None,
            support_url: None,
        }
    }

    #[tokio::test]
    #[ignore]
    async fn sources_music_meta_and_playback_fallback() {
        let dir = std::env::temp_dir().join(format!("lynx-fallback-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = config(dir.clone());
        let store = Store::open(&cfg.database_path()).await.unwrap();
        let procs = ProcessRegistry::new();
        let s = Sources::new(&cfg, store.clone(), procs.clone()).unwrap();
        let owner = ProcOwner::new(10, 20, Some(uuid::Uuid::new_v4()));

        // A pasted official video resolves with the song metadata, playback id unchanged.
        let r = s
            .resolve("https://www.youtube.com/watch?v=JGwWNGJdvx8", 1, &owner)
            .await
            .unwrap();
        let t = &r.tracks[0];
        println!("resolve: {} / {:?} -> {:?}", t.title, t.artists, t.music);
        assert_eq!(t.youtube_id.as_deref(), Some("JGwWNGJdvx8"));
        assert_eq!(t.display_title(), "Shape of You");
        assert_eq!(t.display_artists(), ["Ed Sheeran"]);

        // A Spotify match whose video is gone ("Video unavailable") plays another upload.
        let gone = "00000000000";
        let mut sp = crate::store::test_util::track(gone);
        sp.title = "Never Gonna Give You Up".into();
        sp.artists = vec!["Rick Astley".into()];
        sp.duration_ms = Some(213_000);
        sp.origin = TrackOrigin::Spotify;
        let p = s.prepare(&sp, &owner).await.unwrap();
        println!("fallback: {:?} ({:?})", p.played_youtube_id, p.fallback_reason);
        assert_eq!(p.track.youtube_id.as_deref(), Some(gone));
        let alt = p.played_youtube_id.clone().unwrap();
        assert_ne!(alt, gone);
        assert!(p.page_url.ends_with(&alt) && p.fallback_reason.is_some());
        let cached = store
            .cache_get::<Option<String>>(fallback::CACHE_NS, gone)
            .await
            .unwrap();
        assert_eq!(cached, Some(Some(alt.clone())));
        // Second time: straight to the cached substitute.
        let p = s.prepare(&sp, &owner).await.unwrap();
        assert_eq!(p.played_youtube_id.as_deref(), Some(alt.as_str()));

        // A gone pasted video with its song known falls back to the song first.
        let mut v = crate::store::test_util::track("00000000001");
        v.title = "Rick Astley - Never Gonna Give You Up (Official Video)".into();
        v.artists = vec!["Rick Astley".into()];
        v.duration_ms = Some(213_000);
        v.music = Some(MusicMeta {
            youtube_id: "lYBUbBu4W08".into(),
            title: "Never Gonna Give You Up".into(),
            artists: vec!["Rick Astley".into()],
            album: None,
            isrc: None,
        });
        let p = s.prepare(&v, &owner).await.unwrap();
        println!("video fallback: {:?} ({:?})", p.played_youtube_id, p.fallback_reason);
        assert_eq!(p.played_youtube_id.as_deref(), Some("lYBUbBu4W08"));

        // Nothing confident for gibberish: the original error, and "no alternative" is cached.
        let mut x = crate::store::test_util::track("00000000002");
        x.title = "zqxv wlpk unmatched gibberish title".into();
        x.artists = vec!["Nobody Qzx".into()];
        let err = s.prepare(&x, &owner).await.unwrap_err();
        println!("no alternative: {err}");
        assert!(fallback::fallback_worthy(&err));
        let cached = store
            .cache_get::<Option<String>>(fallback::CACHE_NS, "00000000002")
            .await
            .unwrap();
        assert_eq!(cached, Some(None));

        s.stream_dirs().remove_channel(10, 20).await;
        store.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore]
    async fn sources_apple_resolve_and_match() {
        let dir = std::env::temp_dir().join(format!("lynx-apple-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = config(dir.clone());
        let store = Store::open(&cfg.database_path()).await.unwrap();
        let s = Sources::new(&cfg, store.clone(), ProcessRegistry::new()).unwrap();
        let owner = ProcOwner::new(10, 20, None);
        let r = s.resolve("https://music.apple.com/us/album/moscow-mule/1622045624?i=1622045625", 1, &owner).await.unwrap();
        let t = &r.tracks[0];
        assert_eq!(t.origin, TrackOrigin::AppleMusic);
        assert!(t.youtube_id.is_none());
        let m = s.ensure_youtube(t).await.unwrap();
        println!("apple match: {} -> {:?}", m.title, m.youtube_id);
        assert!(m.youtube_id.is_some());
        let r = s.resolve("https://music.apple.com/us/album/un-verano-sin-ti/1622045624", 5, &owner).await.unwrap();
        assert_eq!((r.playlist_name.as_deref(), r.tracks.len()), (Some("Un Verano Sin Ti"), 5));
        store.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore]
    async fn sources_facade_end_to_end() {
        let dir = std::env::temp_dir().join(format!("lynx-sources-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = config(dir.clone());
        let store = Store::open(&cfg.database_path()).await.unwrap();
        let procs = ProcessRegistry::new();
        let s = Sources::new(&cfg, store.clone(), procs.clone()).unwrap();
        let owner = ProcOwner::new(10, 20, Some(uuid::Uuid::new_v4()));

        let r = s.resolve("bad bunny titi me pregunto", 10, &owner).await.unwrap();
        assert_eq!(r.tracks.len(), 1);
        println!("search resolve: {} - {:?}", r.tracks[0].title, r.tracks[0].artists);

        let r = s.resolve("https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI", 120, &owner).await.unwrap();
        assert_eq!(r.tracks.len(), 120);
        println!("playlist: {:?} ({} tracks)", r.playlist_name, r.tracks.len());

        let r = s.resolve("https://youtu.be/dQw4w9WgXcQ", 10, &owner).await.unwrap();
        let p = s.prepare(&r.tracks[0], &owner).await.unwrap();
        println!("prepare: format {:?} opus {} fallback {}", p.stream.format_id, p.stream.is_opus, p.ytdlp_fallback);
        assert!(p.work_dir.as_ref().unwrap().is_dir());
        assert!(procs.is_empty());

        // An unmatched Spotify-like entry goes through the matcher inside prepare.
        let sp = TrackInfo {
            title: "Never Gonna Give You Up".into(),
            artists: vec!["Rick Astley".into()],
            duration_ms: Some(213_000),
            thumbnail: None,
            url: "https://open.spotify.com/track/x".into(),
            origin: TrackOrigin::Spotify,
            youtube_id: None,
            spotify_id: Some("4PTG3Z6ehGkBFwjybzWkR8".into()),
            isrc: Some("GBARL9300135".into()),
            music: None,
        };
        let p = s.prepare(&sp, &owner).await.unwrap();
        println!("spotify prepare: matched {:?}, format {:?}", p.track.youtube_id, p.stream.format_id);
        assert!(p.track.youtube_id.is_some());

        let res = s.search("never gonna give you up", 10).await.unwrap();
        assert!(res.len() >= 5);
        assert_eq!(res[0].origin, TrackOrigin::YoutubeMusic);
        let l = s.lyrics(&r.tracks[0]).await.unwrap();
        println!("lyrics for video title: synced={}", l.as_ref().is_some_and(|l| l.synced.is_some()));

        s.stream_dirs().remove_channel(10, 20).await;
        store.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
