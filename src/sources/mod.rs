//! Turning user input (URLs, search text, Spotify links) into playable tracks.
//!
//! [`Sources`] is the only entry point the rest of the app needs: `resolve` (input -> tracks),
//! `prepare` (track -> direct stream URL, doing the lazy Spotify match), `search` and `lyrics`.
//! It knows nothing about Discord or songbird.

#![allow(dead_code)]

pub mod detect;
pub mod lyrics;
pub mod matcher;
pub mod spotify;
pub mod text;
pub mod youtube;
pub mod ytdlp;

use std::{path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    config::Config,
    procs::{ProcOwner, ProcessRegistry, StreamDirs},
    store::Store,
};
use detect::Input;
pub use lyrics::Lyrics;
pub use ytdlp::StreamInfo;

/// Generic client identification for third-party APIs. Never put personal data here.
pub const USER_AGENT: &str = "Lynx (https://github.com/pixlgalaxy)";
/// Default cap for playlists, albums and yt-dlp sets.
pub const DEFAULT_LIST_LIMIT: usize = 1000;
const SEARCH_CACHE_NS: &str = "search";

/// Where a track was originally requested from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackOrigin {
    Youtube,
    YoutubeMusic,
    Spotify,
    Other,
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
    lrclib: lyrics::Lrclib,
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
                store,
                ttl: Duration::from_secs(u64::from(config.cache_ttl_days.max(1)) * 24 * 60 * 60),
                yt: youtube::YouTube::new(http.clone()),
                ytdlp: ytdlp::Ytdlp::new(
                    config.bin_dir(),
                    procs,
                    dirs.clone(),
                    http.clone(),
                    config.max_concurrent_extractions,
                    config.ytdlp_cookies_file.clone(),
                ),
                spotify,
                lrclib: lyrics::Lrclib::new(http.clone()),
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
        let limit = limit.max(1);
        let input = match detect::detect(query) {
            Input::SpotifyShortLink(url) => self.expand_short_link(&url).await?,
            other => other,
        };
        let single = |t: TrackInfo| Resolved {
            tracks: vec![t],
            playlist_name: None,
        };
        match input {
            Input::Search(q) if q.is_empty() => Err(SourceError::NotFound("Empty search".into())),
            Input::Search(q) => {
                let songs = self.inner.yt.search_songs(&q, 1).await.unwrap_or_default();
                let found = match songs.into_iter().next() {
                    Some(t) => Some(t),
                    None => self.inner.yt.search_videos(&q, 1).await?.into_iter().next(),
                };
                found
                    .map(single)
                    .ok_or_else(|| SourceError::NotFound(format!("No results for \"{q}\"")))
            }
            Input::YoutubeVideo { id, music } => match self.inner.yt.video(&id, music).await {
                Ok(t) => Ok(single(t)),
                // InnerTube hiccup: let yt-dlp read the metadata instead.
                Err(SourceError::Http(_)) => {
                    let url = format!("https://www.youtube.com/watch?v={id}");
                    let (tracks, _) = self.inner.ytdlp.probe(&url, 1, owner).await?;
                    Ok(Resolved {
                        tracks,
                        playlist_name: None,
                    })
                }
                Err(e) => Err(e),
            },
            Input::YoutubePlaylist { id, music } => {
                let p = self.inner.yt.playlist(&id, music, limit).await?;
                Ok(Resolved {
                    tracks: p.tracks,
                    playlist_name: p.title,
                })
            }
            Input::SpotifyTrack(id) => Ok(single(self.spotify()?.track(&id).await?)),
            Input::SpotifyAlbum(id) => {
                let c = self.spotify()?.album(&id, limit).await?;
                Ok(Resolved {
                    tracks: c.tracks,
                    playlist_name: c.name,
                })
            }
            Input::SpotifyPlaylist(id) => {
                let c = self.spotify()?.playlist(&id, limit).await?;
                if c.tracks.is_empty() {
                    return Err(SourceError::NotFound("That Spotify playlist is empty".into()));
                }
                Ok(Resolved {
                    tracks: c.tracks,
                    playlist_name: c.name,
                })
            }
            Input::SpotifyUnsupported(_) | Input::SpotifyShortLink(_) => Err(SourceError::Unsupported(
                "Only Spotify tracks, albums and playlists can be played".into(),
            )),
            Input::Url(url) => {
                let (tracks, name) = self.inner.ytdlp.probe(&url, limit, owner).await?;
                Ok(Resolved {
                    tracks,
                    playlist_name: name,
                })
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

    /// Spotify -> YouTube match (cached). No-op for tracks that already have a video id.
    pub async fn ensure_youtube(&self, track: &TrackInfo) -> SourceResult<TrackInfo> {
        if track.youtube_id.is_some() || track.origin != TrackOrigin::Spotify {
            return Ok(track.clone());
        }
        matcher::find(&self.inner.yt, &self.inner.store, self.inner.ttl, track).await
    }

    /// Everything needed to start playing `track` now: lazy Spotify match + stream URL.
    pub async fn prepare(&self, track: &TrackInfo, owner: &ProcOwner) -> SourceResult<Playable> {
        let track = self.ensure_youtube(track).await?;
        let (page_url, key) = match &track.youtube_id {
            Some(id) => (format!("https://www.youtube.com/watch?v={id}"), id.clone()),
            None => (track.url.clone(), track.url.clone()),
        };
        let stream = self.inner.ytdlp.resolve_stream(&page_url, &key, owner).await?;
        let work_dir = owner
            .track_uid
            .map(|t| self.inner.dirs.track_path(owner.guild_id, owner.voice_channel_id, t));
        Ok(Playable {
            ytdlp_fallback: stream.needs_ytdlp,
            track,
            stream,
            page_url,
            work_dir,
        })
    }

    /// Forget a cached stream URL (e.g. after the CDN answered 403).
    pub fn invalidate_stream(&self, track: &TrackInfo) {
        let key = track.youtube_id.as_deref().unwrap_or(&track.url);
        self.inner.ytdlp.invalidate(key);
    }

    /// Search for the panel / autocomplete: YouTube Music songs first, then regular videos.
    pub async fn search(&self, query: &str, limit: usize) -> SourceResult<Vec<TrackInfo>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 25);
        let key = format!("{}|{limit}", text::normalize(q));
        if let Ok(Some(hit)) = self
            .inner
            .store
            .cache_get::<Vec<TrackInfo>>(SEARCH_CACHE_NS, &key)
            .await
        {
            return Ok(hit);
        }
        let (songs, videos) = tokio::join!(
            self.inner.yt.search_songs(q, limit),
            self.inner.yt.search_videos(q, limit)
        );
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
            let _ = self
                .inner
                .store
                .cache_put(SEARCH_CACHE_NS, &key, &out, self.inner.ttl)
                .await;
        }
        Ok(out)
    }

    pub async fn lyrics(&self, track: &TrackInfo) -> SourceResult<Option<Lyrics>> {
        self.inner.lrclib.lyrics(&self.inner.store, self.inner.ttl, track).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(data_dir: PathBuf) -> Config {
        Config {
            discord_token: String::new(),
            discord_client_id: String::new(),
            discord_client_secret: String::new(),
            node_tokens: Vec::new(),
            public_url: String::new(),
            http_port: 0,
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
            ytdlp_cookies_file: None,
            empty_channel_timeout_secs: 0,
            max_concurrent_extractions: 2,
            stream_cache_max_mb: 100,
            cache_ttl_days: 7,
            landing_url: None,
            terms_url: None,
            privacy_url: None,
            support_url: None,
        }
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

        let r = s
            .resolve(
                "https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
                120,
                &owner,
            )
            .await
            .unwrap();
        assert_eq!(r.tracks.len(), 120);
        println!("playlist: {:?} ({} tracks)", r.playlist_name, r.tracks.len());

        let r = s.resolve("https://youtu.be/dQw4w9WgXcQ", 10, &owner).await.unwrap();
        let p = s.prepare(&r.tracks[0], &owner).await.unwrap();
        println!(
            "prepare: format {:?} opus {} fallback {}",
            p.stream.format_id, p.stream.is_opus, p.ytdlp_fallback
        );
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
        };
        let p = s.prepare(&sp, &owner).await.unwrap();
        println!(
            "spotify prepare: matched {:?}, format {:?}",
            p.track.youtube_id, p.stream.format_id
        );
        assert!(p.track.youtube_id.is_some());

        let res = s.search("never gonna give you up", 10).await.unwrap();
        assert!(res.len() >= 5);
        assert_eq!(res[0].origin, TrackOrigin::YoutubeMusic);
        let l = s.lyrics(&r.tracks[0]).await.unwrap();
        println!(
            "lyrics for video title: synced={}",
            l.as_ref().is_some_and(|l| l.synced.is_some())
        );

        s.stream_dirs().remove_channel(10, 20).await;
        store.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
