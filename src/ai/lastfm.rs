//! Last.fm client: top tags and similar tracks/artists. Results are cached in the store.

use std::{sync::Arc, time::Duration};

use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::{limiter::RateLimiter, ApiError};
use crate::{sources::text::normalize, store::Store};

const BASE: &str = "https://ws.audioscrobbler.com/2.0/";
const CACHE_NS: &str = "lastfm";

/// A track suggested by Last.fm (not resolved to YouTube yet).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LfmTrack {
    pub artist: String,
    pub title: String,
    pub duration_ms: Option<u64>,
    /// 0..1 similarity (or popularity rank score for top tracks).
    pub score: f64,
}

#[derive(Clone)]
pub struct LastFm {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    key: String,
    store: Store,
    ttl: Duration,
    limiter: RateLimiter,
    base: String,
}

impl LastFm {
    pub fn new(http: reqwest::Client, key: String, store: Store, ttl: Duration) -> Self {
        Self::with_base(http, key, store, ttl, BASE.into())
    }

    pub fn with_base(http: reqwest::Client, key: String, store: Store, ttl: Duration, base: String) -> Self {
        // Last.fm asks for at most ~5 requests per second per key.
        Self {
            inner: Arc::new(Inner {
                http,
                key,
                store,
                ttl,
                limiter: RateLimiter::per_second(5),
                base,
            }),
        }
    }

    /// `track.getTopTags`: raw (tag, weight 0..1).
    pub async fn track_tags(
        &self,
        artist: &str,
        title: &str,
        c: &CancellationToken,
    ) -> Result<Vec<(String, f64)>, ApiError> {
        let key = format!("tt|{}|{}", normalize(artist), normalize(title));
        self.cached(&key, c, || async {
            let v: TopTagsResp = self
                .call("track.gettoptags", &[("artist", artist), ("track", title)], c)
                .await?;
            Ok(v.tags())
        })
        .await
    }

    /// `artist.getTopTags`: raw (tag, weight 0..1).
    pub async fn artist_tags(&self, artist: &str, c: &CancellationToken) -> Result<Vec<(String, f64)>, ApiError> {
        let key = format!("at|{}", normalize(artist));
        self.cached(&key, c, || async {
            let v: TopTagsResp = self.call("artist.gettoptags", &[("artist", artist)], c).await?;
            Ok(v.tags())
        })
        .await
    }

    /// `track.getSimilar`.
    pub async fn similar_tracks(
        &self,
        artist: &str,
        title: &str,
        limit: u32,
        c: &CancellationToken,
    ) -> Result<Vec<LfmTrack>, ApiError> {
        let key = format!("st|{}|{}|{limit}", normalize(artist), normalize(title));
        let lim = limit.to_string();
        self.cached(&key, c, || async {
            let v: SimilarTracksResp = self
                .call(
                    "track.getsimilar",
                    &[("artist", artist), ("track", title), ("limit", &lim)],
                    c,
                )
                .await?;
            Ok(v.similartracks
                .map(|s| s.track.0)
                .unwrap_or_default()
                .into_iter()
                .filter_map(RawTrack::into_track)
                .collect())
        })
        .await
    }

    /// `artist.getSimilar`: (artist, match 0..1).
    pub async fn similar_artists(
        &self,
        artist: &str,
        limit: u32,
        c: &CancellationToken,
    ) -> Result<Vec<(String, f64)>, ApiError> {
        let key = format!("sa|{}|{limit}", normalize(artist));
        let lim = limit.to_string();
        self.cached(&key, c, || async {
            let v: SimilarArtistsResp = self
                .call("artist.getsimilar", &[("artist", artist), ("limit", &lim)], c)
                .await?;
            Ok(v.similarartists
                .map(|s| s.artist.0)
                .unwrap_or_default()
                .into_iter()
                .filter(|a| !a.name.trim().is_empty())
                .map(|a| (a.name, a.score.0.clamp(0.0, 1.0)))
                .collect())
        })
        .await
    }

    /// `artist.getTopTracks` (turns similar artists into playable suggestions).
    pub async fn artist_top_tracks(
        &self,
        artist: &str,
        limit: u32,
        c: &CancellationToken,
    ) -> Result<Vec<LfmTrack>, ApiError> {
        let key = format!("tp|{}|{limit}", normalize(artist));
        let lim = limit.to_string();
        self.cached(&key, c, || async {
            let v: TopTracksResp = self
                .call("artist.gettoptracks", &[("artist", artist), ("limit", &lim)], c)
                .await?;
            let tracks = v.toptracks.map(|s| s.track.0).unwrap_or_default();
            let n = tracks.len().max(1) as f64;
            Ok(tracks
                .into_iter()
                .enumerate()
                .filter_map(|(i, t)| {
                    let mut t = t.into_track()?;
                    t.score = 1.0 - i as f64 / n;
                    Some(t)
                })
                .collect())
        })
        .await
    }

    /// `tag.getTopTracks`: the most popular tracks for a genre tag (prompt-driven candidates).
    pub async fn tag_top_tracks(
        &self,
        tag: &str,
        limit: u32,
        c: &CancellationToken,
    ) -> Result<Vec<LfmTrack>, ApiError> {
        let key = format!("tg|{}|{limit}", normalize(tag));
        let lim = limit.to_string();
        self.cached(&key, c, || async {
            let v: TagTracksResp = self
                .call("tag.gettoptracks", &[("tag", tag), ("limit", &lim)], c)
                .await?;
            let tracks = v.tracks.map(|s| s.track.0).unwrap_or_default();
            let n = tracks.len().max(1) as f64;
            Ok(tracks
                .into_iter()
                .enumerate()
                .filter_map(|(i, t)| {
                    let mut t = t.into_track()?;
                    t.score = 1.0 - i as f64 / n;
                    Some(t)
                })
                .collect())
        })
        .await
    }

    async fn cached<T, F, Fut>(&self, key: &str, c: &CancellationToken, fetch: F) -> Result<T, ApiError>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, ApiError>>,
    {
        if let Ok(Some(hit)) = self.inner.store.cache_get::<T>(CACHE_NS, key).await {
            return Ok(hit);
        }
        if c.is_cancelled() {
            return Err(ApiError::Cancelled);
        }
        let v = fetch().await?;
        let _ = self.inner.store.cache_put(CACHE_NS, key, &v, self.inner.ttl).await;
        Ok(v)
    }

    async fn call<T: DeserializeOwned + Default>(
        &self,
        method: &str,
        params: &[(&str, &str)],
        c: &CancellationToken,
    ) -> Result<T, ApiError> {
        self.inner.limiter.acquire(c).await.map_err(|_| ApiError::Cancelled)?;
        let mut q: Vec<(&str, &str)> = vec![
            ("method", method),
            ("api_key", &self.inner.key),
            ("format", "json"),
            ("autocorrect", "1"),
        ];
        q.extend_from_slice(params);
        let req = self
            .inner
            .http
            .get(&self.inner.base)
            .query(&q)
            .timeout(Duration::from_secs(15))
            .send();
        let res = tokio::select! {
            r = req => r.map_err(ApiError::from)?,
            _ = c.cancelled() => return Err(ApiError::Cancelled),
        };
        let status = res.status();
        let body = tokio::select! {
            b = res.text() => b.map_err(ApiError::from)?,
            _ = c.cancelled() => return Err(ApiError::Cancelled),
        };
        // Errors come back as {"error": n, "message": "..."}, sometimes with HTTP 200.
        if let Ok(e) = serde_json::from_str::<ErrorResp>(&body) {
            return match e.error {
                // 6: not found. A missing track is a valid, cacheable answer.
                6 => Ok(T::default()),
                29 => Err(ApiError::Http("Last.fm rate limit".into())),
                n => Err(ApiError::Api(format!("Last.fm error {n}: {}", e.message))),
            };
        }
        if !status.is_success() {
            return Err(ApiError::Http(format!("Last.fm HTTP {status}")));
        }
        serde_json::from_str(&body).map_err(|e| {
            debug!(error = %e, method, "unexpected Last.fm response");
            ApiError::Api("unexpected Last.fm response".into())
        })
    }
}

#[derive(Deserialize)]
struct ErrorResp {
    error: i64,
    #[serde(default)]
    message: String,
}

/// Last.fm turns one-element arrays into objects; accept both.
#[derive(Debug)]
struct OneOrMany<T>(Vec<T>);

impl<T> Default for OneOrMany<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OneOrMany<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw<T> {
            Many(Vec<T>),
            One(T),
            Other(serde_json::Value),
        }
        Ok(match Raw::<T>::deserialize(d)? {
            Raw::Many(v) => Self(v),
            Raw::One(t) => Self(vec![t]),
            Raw::Other(_) => Self(Vec::new()),
        })
    }
}

/// Numbers that may come as strings ("0.53", "100").
#[derive(Debug, Default, Clone, Copy)]
struct Num(f64);

impl<'de> Deserialize<'de> for Num {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            N(f64),
            S(String),
            Other(serde_json::Value),
        }
        Ok(match Raw::deserialize(d)? {
            Raw::N(n) => Num(n),
            Raw::S(s) => Num(s.trim().parse().unwrap_or(0.0)),
            Raw::Other(_) => Num(0.0),
        })
    }
}

#[derive(Deserialize, Default)]
struct TopTagsResp {
    toptags: Option<TagList>,
}

#[derive(Deserialize, Default)]
struct TagList {
    #[serde(default)]
    tag: OneOrMany<RawTag>,
}

#[derive(Deserialize)]
struct RawTag {
    name: String,
    #[serde(default)]
    count: Num,
}

impl TopTagsResp {
    fn tags(self) -> Vec<(String, f64)> {
        self.toptags
            .map(|t| t.tag.0)
            .unwrap_or_default()
            .into_iter()
            .take(20)
            .map(|t| (t.name, (t.count.0 / 100.0).clamp(0.0, 1.0)))
            .filter(|(n, w)| !n.trim().is_empty() && *w >= 0.05)
            .collect()
    }
}

#[derive(Deserialize, Default)]
struct SimilarTracksResp {
    similartracks: Option<TrackList>,
}

#[derive(Deserialize, Default)]
struct TopTracksResp {
    toptracks: Option<TrackList>,
}

#[derive(Deserialize, Default)]
struct TagTracksResp {
    tracks: Option<TrackList>,
}

#[derive(Deserialize, Default)]
struct TrackList {
    #[serde(default)]
    track: OneOrMany<RawTrack>,
}

#[derive(Deserialize)]
struct RawTrack {
    name: String,
    #[serde(default, rename = "match")]
    score: Num,
    #[serde(default)]
    duration: Num,
    artist: Option<RawArtistRef>,
}

#[derive(Deserialize)]
struct RawArtistRef {
    name: String,
}

impl RawTrack {
    fn into_track(self) -> Option<LfmTrack> {
        let artist = self.artist?.name;
        if artist.trim().is_empty() || self.name.trim().is_empty() {
            return None;
        }
        // getSimilar reports seconds, getTopTracks sometimes "0".
        let d = self.duration.0;
        let duration_ms = (d > 0.0).then_some(if d > 10_000.0 { d as u64 } else { (d * 1000.0) as u64 });
        Some(LfmTrack {
            artist,
            title: self.name,
            duration_ms,
            score: self.score.0.clamp(0.0, 1.0),
        })
    }
}

#[derive(Deserialize, Default)]
struct SimilarArtistsResp {
    similarartists: Option<ArtistList>,
}

#[derive(Deserialize, Default)]
struct ArtistList {
    #[serde(default)]
    artist: OneOrMany<RawArtist>,
}

#[derive(Deserialize)]
struct RawArtist {
    name: String,
    #[serde(default, rename = "match")]
    score: Num,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tag_and_similar_shapes() {
        let v: TopTagsResp = serde_json::from_str(
            r#"{"toptags":{"tag":[{"count":100,"name":"reggaeton"},{"count":"40","name":"latin"},{"count":1,"name":"x"}]}}"#,
        )
        .unwrap();
        assert_eq!(v.tags(), vec![("reggaeton".into(), 1.0), ("latin".into(), 0.4)]);
        let v: TopTagsResp = serde_json::from_str(r#"{"toptags":{"tag":{"count":50,"name":"pop"}}}"#).unwrap();
        assert_eq!(v.tags().len(), 1);
        let v: TopTagsResp = serde_json::from_str(r#"{"toptags":{"tag":"","@attr":{}}}"#).unwrap();
        assert!(v.tags().is_empty());

        let s: SimilarTracksResp = serde_json::from_str(
            r#"{"similartracks":{"track":[{"name":"Song","match":0.8,"duration":200,"artist":{"name":"A"}}]}}"#,
        )
        .unwrap();
        let t = s
            .similartracks
            .unwrap()
            .track
            .0
            .into_iter()
            .next()
            .unwrap()
            .into_track()
            .unwrap();
        assert_eq!(
            t,
            LfmTrack {
                artist: "A".into(),
                title: "Song".into(),
                duration_ms: Some(200_000),
                score: 0.8
            }
        );

        let a: SimilarArtistsResp =
            serde_json::from_str(r#"{"similarartists":{"artist":[{"name":"B","match":"0.5"}]}}"#).unwrap();
        assert_eq!(a.similarartists.unwrap().artist.0[0].score.0, 0.5);
    }

    /// Live: needs LASTFM_API_KEY. `cargo test lastfm_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn lastfm_live() {
        let Some(key) = std::env::var("LASTFM_API_KEY").ok().filter(|k| !k.is_empty()) else {
            eprintln!("LASTFM_API_KEY not set, skipping");
            return;
        };
        let store = Store::open_in_memory().await.unwrap();
        let lfm = LastFm::new(reqwest::Client::new(), key, store, Duration::from_secs(3600));
        let c = CancellationToken::new();
        for (artist, title) in [
            ("Bad Bunny", "Tití Me Preguntó"),
            ("Radiohead", "Creep"),
            ("Peso Pluma", "Ella Baila Sola"),
        ] {
            let start = std::time::Instant::now();
            let raw = lfm.track_tags(artist, title, &c).await.unwrap();
            let (genres, unknown) = crate::ai::taxonomy::normalize_weighted(&raw, 5);
            println!(
                "{artist} - {title}: {} raw tags in {:?} -> {genres:?} (unknown {unknown:?})",
                raw.len(),
                start.elapsed()
            );
            let a = lfm.artist_tags(artist, &c).await.unwrap();
            println!(
                "  artist tags -> {:?}",
                crate::ai::taxonomy::normalize_weighted(&a, 5).0
            );
        }
        let start = std::time::Instant::now();
        let sim = lfm
            .similar_tracks("Bad Bunny", "Tití Me Preguntó", 8, &c)
            .await
            .unwrap();
        println!(
            "similar tracks ({:?}): {:?}",
            start.elapsed(),
            sim.iter()
                .map(|t| format!("{} - {}", t.artist, t.title))
                .collect::<Vec<_>>()
        );
        let start = std::time::Instant::now();
        let again = lfm
            .similar_tracks("Bad Bunny", "Tití Me Preguntó", 8, &c)
            .await
            .unwrap();
        println!("cached similar tracks in {:?}", start.elapsed());
        assert_eq!(sim, again);
        let sa = lfm.similar_artists("Bad Bunny", 4, &c).await.unwrap();
        println!("similar artists: {sa:?}");
        assert!(!sa.is_empty());
        let top = lfm.artist_top_tracks(&sa[0].0, 3, &c).await.unwrap();
        println!(
            "top tracks of {}: {:?}",
            sa[0].0,
            top.iter().map(|t| &t.title).collect::<Vec<_>>()
        );
        assert!(!top.is_empty());
        let tag = lfm.tag_top_tracks("rock en español", 10, &c).await.unwrap();
        println!(
            "tag top tracks (rock en espanol): {:?}",
            tag.iter()
                .map(|t| format!("{} - {}", t.artist, t.title))
                .collect::<Vec<_>>()
        );
        assert!(!tag.is_empty());
        let missing = lfm
            .track_tags("zzqqxx nonexistent", "nothing here 123", &c)
            .await
            .unwrap();
        assert!(missing.is_empty());
    }
}
