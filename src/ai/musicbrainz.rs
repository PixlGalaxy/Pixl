//! MusicBrainz client (genre fallback): ISRC -> recording -> genres/tags, else a recording search.
//! Strictly 1 request per second and a User-Agent with contact info (from the config).

use std::{sync::Arc, time::Duration};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{limiter::RateLimiter, ApiError};
use crate::{sources::text::normalize, store::Store};

const BASE: &str = "https://musicbrainz.org/ws/2";
const CACHE_NS: &str = "musicbrainz";

#[derive(Clone)]
pub struct MusicBrainz {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    user_agent: String,
    store: Store,
    ttl: Duration,
    limiter: RateLimiter,
}

impl MusicBrainz {
    /// None when no User-Agent is configured: MusicBrainz is skipped entirely.
    pub fn new(http: reqwest::Client, user_agent: Option<&str>, store: Store, ttl: Duration) -> Option<Self> {
        let ua = user_agent.map(str::trim).filter(|s| !s.is_empty())?;
        // A little over a second, so clock jitter never makes two requests land in the same second.
        let limiter = RateLimiter::new(Duration::from_millis(1100));
        Some(Self {
            inner: Arc::new(Inner {
                http,
                user_agent: ua.to_owned(),
                store,
                ttl,
                limiter,
            }),
        })
    }

    /// Raw (tag, weight 0..1) for a recording, by ISRC when known, else by search.
    pub async fn recording_tags(
        &self,
        isrc: Option<&str>,
        artist: &str,
        title: &str,
        c: &CancellationToken,
    ) -> Result<Vec<(String, f64)>, ApiError> {
        let key = match isrc {
            Some(i) => format!("isrc|{}", i.to_ascii_uppercase()),
            None => format!("q|{}|{}", normalize(artist), normalize(title)),
        };
        if let Ok(Some(hit)) = self.inner.store.cache_get::<Vec<(String, f64)>>(CACHE_NS, &key).await {
            return Ok(hit);
        }
        let mut tags = Vec::new();
        let mut recording_id = None;
        if let Some(isrc) = isrc {
            let r: IsrcResp = self
                .get(
                    &format!("{BASE}/isrc/{}?inc=genres+tags&fmt=json", isrc.to_ascii_uppercase()),
                    c,
                )
                .await?;
            for rec in &r.recordings {
                tags.extend(rec.tags());
            }
            recording_id = r.recordings.first().map(|r| r.id.clone());
        }
        if tags.is_empty() && recording_id.is_none() {
            let q = format!(
                "recording:\"{}\" AND artist:\"{}\"",
                lucene_escape(title),
                lucene_escape(artist)
            );
            let url = reqwest::Url::parse_with_params(
                &format!("{BASE}/recording"),
                &[("query", q.as_str()), ("limit", "5"), ("fmt", "json")],
            )
            .map_err(|e| ApiError::Api(e.to_string()))?;
            let r: SearchResp = self.get(url.as_str(), c).await?;
            recording_id = r
                .recordings
                .into_iter()
                .find(|r| r.score.unwrap_or(0) >= 90)
                .map(|r| r.id);
        }
        if tags.is_empty() {
            if let Some(id) = recording_id {
                let rec: Recording = self
                    .get(
                        &format!("{BASE}/recording/{id}?inc=genres+tags+artist-credits&fmt=json"),
                        c,
                    )
                    .await?;
                tags = rec.tags();
                if tags.is_empty() {
                    if let Some(aid) = rec.artist_credit.first().map(|a| a.artist.id.clone()) {
                        let a: Recording = self
                            .get(&format!("{BASE}/artist/{aid}?inc=genres+tags&fmt=json"), c)
                            .await?;
                        tags = a.tags();
                    }
                }
            }
        }
        let _ = self.inner.store.cache_put(CACHE_NS, &key, &tags, self.inner.ttl).await;
        Ok(tags)
    }

    async fn get<T: DeserializeOwned + Default>(&self, url: &str, c: &CancellationToken) -> Result<T, ApiError> {
        self.inner.limiter.acquire(c).await.map_err(|_| ApiError::Cancelled)?;
        let req = self
            .inner
            .http
            .get(url)
            .header(reqwest::header::USER_AGENT, &self.inner.user_agent)
            .timeout(Duration::from_secs(15))
            .send();
        let res = tokio::select! {
            r = req => r.map_err(ApiError::from)?,
            _ = c.cancelled() => return Err(ApiError::Cancelled),
        };
        match res.status().as_u16() {
            404 => return Ok(T::default()),
            200..=299 => {}
            s => return Err(ApiError::Http(format!("MusicBrainz HTTP {s}"))),
        }
        tokio::select! {
            r = res.json::<T>() => r.map_err(ApiError::from),
            _ = c.cancelled() => Err(ApiError::Cancelled),
        }
    }
}

fn lucene_escape(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '"' | '\\')).collect()
}

#[derive(Deserialize, Default)]
struct IsrcResp {
    #[serde(default)]
    recordings: Vec<Recording>,
}

#[derive(Deserialize, Default)]
struct SearchResp {
    #[serde(default)]
    recordings: Vec<SearchHit>,
}

#[derive(Deserialize)]
struct SearchHit {
    id: String,
    score: Option<u32>,
}

#[derive(Deserialize, Serialize, Default)]
struct Recording {
    #[serde(default)]
    id: String,
    #[serde(default)]
    genres: Vec<MbTag>,
    #[serde(default)]
    tags: Vec<MbTag>,
    #[serde(default, rename = "artist-credit")]
    artist_credit: Vec<ArtistCredit>,
}

#[derive(Deserialize, Serialize)]
struct ArtistCredit {
    artist: ArtistRef,
}

#[derive(Deserialize, Serialize)]
struct ArtistRef {
    id: String,
}

#[derive(Deserialize, Serialize, Clone)]
struct MbTag {
    name: String,
    #[serde(default)]
    count: i64,
}

impl Recording {
    /// Genres first (curated), then folksonomy tags; weights relative to the top count.
    fn tags(&self) -> Vec<(String, f64)> {
        let all: Vec<&MbTag> = self
            .genres
            .iter()
            .chain(self.tags.iter())
            .filter(|t| t.count > 0)
            .collect();
        let top = all.iter().map(|t| t.count).max().unwrap_or(1).max(1) as f64;
        all.into_iter()
            .map(|t| (t.name.clone(), (t.count as f64 / top).clamp(0.1, 1.0)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_without_user_agent() {
        let store = Store::open_in_memory().await.unwrap();
        let http = reqwest::Client::new();
        assert!(MusicBrainz::new(http.clone(), None, store.clone(), Duration::ZERO).is_none());
        assert!(MusicBrainz::new(http.clone(), Some("  "), store.clone(), Duration::ZERO).is_none());
        assert!(MusicBrainz::new(http, Some("Lynx/1.0 (x@example.com)"), store, Duration::ZERO).is_some());
    }

    #[test]
    fn recording_tags_weighting() {
        let r: Recording = serde_json::from_str(
            r#"{"id":"x","genres":[{"name":"reggaeton","count":4}],"tags":[{"name":"latin","count":2},{"name":"z","count":0}]}"#,
        )
        .unwrap();
        assert_eq!(r.tags(), vec![("reggaeton".into(), 1.0), ("latin".into(), 0.5)]);
    }
}
