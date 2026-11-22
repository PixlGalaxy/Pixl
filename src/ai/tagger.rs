//! Background genre tagging: Last.fm (track, then artist) -> MusicBrainz -> Ollama, normalized to the
//! taxonomy. Never blocks playback; waits on a `Notify` when the queue is empty.

use std::{collections::HashMap, sync::Arc, time::Duration};

use serde::Deserialize;
use serde_json::json;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::{
    lastfm::LastFm,
    musicbrainz::MusicBrainz,
    ollama::Ollama,
    taxonomy::{self, GENRES},
    ApiError,
};
use crate::{
    sources::{text::normalize, TrackOrigin},
    store::{CatalogTrack, Store, Tag, TagSource},
};

/// Transient failures before a track is given up on.
pub const MAX_ATTEMPTS: u32 = 4;
const BATCH: u32 = 25;
const IDLE_WAIT: Duration = Duration::from_secs(300);
const ERROR_BACKOFF: Duration = Duration::from_secs(30);
/// Pause between tracks so the tagger never takes the whole Last.fm budget from radio jobs.
const PACING: Duration = Duration::from_millis(400);
const MAX_GENRES: usize = 5;
const TAG_MAP_NS: &str = "tag_map";
const TAG_MAP_TTL: Duration = Duration::from_secs(180 * 86_400);

#[derive(Clone)]
pub struct Tagger {
    pub(crate) store: Store,
    pub(crate) lastfm: Option<LastFm>,
    pub(crate) musicbrainz: Option<MusicBrainz>,
    pub(crate) ollama: Option<Ollama>,
    pub(crate) artist_ttl: Duration,
    pub(crate) wake: Arc<Notify>,
}

#[derive(Debug)]
enum TagError {
    Cancelled,
    /// Ollama was busy with a radio job; try again later without counting an attempt.
    Busy,
    Api(String),
}

impl From<ApiError> for TagError {
    fn from(e: ApiError) -> Self {
        match e {
            ApiError::Cancelled => Self::Cancelled,
            other => Self::Api(other.to_string()),
        }
    }
}

impl Tagger {
    pub fn is_useful(&self) -> bool {
        self.lastfm.is_some() || self.musicbrainz.is_some() || self.ollama.is_some()
    }

    pub async fn run(self, stop: CancellationToken) {
        loop {
            let batch = match self.store.pending_tag_tracks(BATCH).await {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "tag queue read failed");
                    Vec::new()
                }
            };
            if batch.is_empty() {
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = tokio::time::sleep(IDLE_WAIT) => {}
                    _ = stop.cancelled() => return,
                }
                continue;
            }
            for t in batch {
                if stop.is_cancelled() {
                    return;
                }
                let pause = match self.tag_one(&t, &stop).await {
                    Ok(tags) => {
                        debug!(track = %t.key, tags = tags.len(), "tagged");
                        if let Err(e) = self.store.write_track_tags(t.id, &tags).await {
                            warn!(error = %e, "write_track_tags failed");
                        }
                        PACING
                    }
                    Err(TagError::Cancelled) => return,
                    Err(TagError::Busy) => ERROR_BACKOFF,
                    Err(TagError::Api(e)) => {
                        debug!(track = %t.key, error = %e, "tagging failed");
                        let _ = self.store.record_tag_failure(t.id, MAX_ATTEMPTS).await;
                        ERROR_BACKOFF
                    }
                };
                tokio::select! {
                    _ = tokio::time::sleep(pause) => {}
                    _ = stop.cancelled() => return,
                }
                if pause == ERROR_BACKOFF {
                    // Re-read the queue: order changed (attempt counts) and the API may be back.
                    break;
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn tag_one_for_test(&self, t: &CatalogTrack, c: &CancellationToken) -> Vec<Tag> {
        self.tag_one(t, c).await.unwrap_or_default()
    }

    async fn tag_one(&self, t: &CatalogTrack, c: &CancellationToken) -> Result<Vec<Tag>, TagError> {
        let (artist, title) = lookup_names(t);
        let mut genres: Genres = HashMap::new();

        if let Some(lfm) = &self.lastfm {
            if !title.is_empty() && !artist.is_empty() {
                let raw = lfm.track_tags(&artist, &title, c).await?;
                let (g, unknown) = taxonomy::normalize_weighted(&raw, MAX_GENRES);
                merge(g, 1.0, TagSource::Lastfm, &mut genres);
                merge(self.map_unknown(&unknown, c).await, 0.8, TagSource::Lastfm, &mut genres);
            }
            if genres.len() < 2 && !artist.is_empty() {
                merge(
                    self.artist_genres(lfm, &artist, c).await?,
                    0.6,
                    TagSource::Lastfm,
                    &mut genres,
                );
            }
        }
        if genres.is_empty() {
            if let Some(mb) = &self.musicbrainz {
                let raw = mb.recording_tags(t.isrc.as_deref(), &artist, &title, c).await?;
                let (g, unknown) = taxonomy::normalize_weighted(&raw, MAX_GENRES);
                merge(g, 1.0, TagSource::Musicbrainz, &mut genres);
                merge(
                    self.map_unknown(&unknown, c).await,
                    0.8,
                    TagSource::Musicbrainz,
                    &mut genres,
                );
            }
        }
        if genres.is_empty() {
            if let Some(o) = &self.ollama {
                // Background work never waits for the GPU: radio jobs come first.
                let Some(permit) = o.try_acquire() else {
                    return Err(TagError::Busy);
                };
                let g = classify(o, &permit, &artist, &title, c).await?;
                merge(g, 0.7, TagSource::Ollama, &mut genres);
            }
        }

        let weights: HashMap<&'static str, f64> = genres.iter().map(|(g, (w, _))| (*g, *w)).collect();
        Ok(taxonomy::finish(weights, MAX_GENRES)
            .into_iter()
            .map(|(g, w)| Tag {
                tag: g.to_owned(),
                weight: (w * 1000.0).round() / 1000.0,
                source: genres[g].1,
            })
            .collect())
    }

    /// Artist genres, from the `artist_tags` cache when fresh.
    async fn artist_genres(
        &self,
        lfm: &LastFm,
        artist: &str,
        c: &CancellationToken,
    ) -> Result<Vec<(&'static str, f64)>, TagError> {
        if let Ok(Some(cached)) = self.store.artist_tags(artist, self.artist_ttl).await {
            return Ok(cached
                .iter()
                .filter_map(|t| Some((taxonomy::canonical(&t.tag)?, t.weight)))
                .collect());
        }
        let raw = lfm.artist_tags(artist, c).await?;
        let (g, _) = taxonomy::normalize_weighted(&raw, MAX_GENRES);
        let tags: Vec<Tag> = g
            .iter()
            .map(|(g, w)| Tag {
                tag: (*g).into(),
                weight: *w,
                source: TagSource::Lastfm,
            })
            .collect();
        if !tags.is_empty() {
            let _ = self.store.put_artist_tags(artist, &tags).await;
        }
        Ok(g)
    }

    /// Map unknown raw tags with Ollama once; results (including "no genre") are cached.
    async fn map_unknown(&self, unknown: &[(String, f64)], c: &CancellationToken) -> Vec<(&'static str, f64)> {
        let mut out = Vec::new();
        let mut ask: Vec<&(String, f64)> = Vec::new();
        for u in unknown.iter().filter(|(_, w)| *w >= 0.3).take(8) {
            match self.store.cache_get::<Vec<String>>(TAG_MAP_NS, &u.0).await {
                Ok(Some(gs)) => out.extend(gs.iter().filter_map(|g| taxonomy::canonical(g)).map(|g| (g, u.1))),
                _ => ask.push(u),
            }
        }
        let (Some(o), false) = (&self.ollama, ask.is_empty()) else {
            return out;
        };
        let Some(permit) = o.try_acquire() else { return out };
        #[derive(Deserialize)]
        struct Resp {
            mappings: Vec<Mapping>,
        }
        #[derive(Deserialize)]
        struct Mapping {
            tag: String,
            #[serde(default)]
            genres: Vec<String>,
        }
        let schema = json!({
            "type": "object",
            "properties": { "mappings": { "type": "array", "items": {
                "type": "object",
                "properties": {
                    "tag": { "type": "string" },
                    "genres": { "type": "array", "items": { "type": "string", "enum": GENRES }, "maxItems": 2 }
                },
                "required": ["tag", "genres"]
            }}},
            "required": ["mappings"]
        });
        let tags: Vec<&str> = ask.iter().map(|(t, _)| t.as_str()).collect();
        let user = format!(
            "Map each music tag to at most 2 genres from the allowed list, or an empty list if it is not a genre \
             (moods, places, years, opinions). Tags: {}",
            serde_json::to_string(&tags).unwrap_or_default()
        );
        let system = "You classify music tags into a fixed genre taxonomy. Answer only with JSON.";
        match o.chat_json::<Resp>(&permit, system, &user, &schema, c).await {
            Ok(r) => {
                let by_tag: HashMap<String, Vec<String>> =
                    r.mappings.into_iter().map(|m| (normalize(&m.tag), m.genres)).collect();
                for (tag, w) in ask {
                    let gs: Vec<&'static str> = by_tag
                        .get(tag)
                        .map(|v| v.iter().filter_map(|g| taxonomy::canonical(g)).collect())
                        .unwrap_or_default();
                    let _ = self.store.cache_put(TAG_MAP_NS, tag, &gs, TAG_MAP_TTL).await;
                    out.extend(gs.into_iter().map(|g| (g, *w)));
                }
            }
            Err(e) => debug!(error = %e, "tag mapping failed"),
        }
        out
    }
}

type Genres = HashMap<&'static str, (f64, TagSource)>;

/// Keep the heaviest weight (and its source) per genre.
fn merge(list: Vec<(&'static str, f64)>, scale: f64, src: TagSource, genres: &mut Genres) {
    for (g, w) in list {
        let e = genres.entry(g).or_insert((0.0, src));
        if w * scale > e.0 {
            *e = (w * scale, src);
        }
    }
}

/// Last resort: ask the model for 1-3 genres from the closed list.
async fn classify(
    o: &Ollama,
    permit: &super::ollama::OllamaPermit,
    artist: &str,
    title: &str,
    c: &CancellationToken,
) -> Result<Vec<(&'static str, f64)>, TagError> {
    #[derive(Deserialize)]
    struct Resp {
        genres: Vec<String>,
    }
    let schema = json!({
        "type": "object",
        "properties": { "genres": { "type": "array", "items": { "type": "string", "enum": GENRES }, "minItems": 1, "maxItems": 3 } },
        "required": ["genres"]
    });
    let user =
        format!("Song: \"{title}\" by \"{artist}\". Pick 1 to 3 genres from the allowed list, most fitting first.");
    let r: Resp = o
        .chat_json(
            permit,
            "You are a music librarian. Answer only with JSON.",
            &user,
            &schema,
            c,
        )
        .await?;
    let mut out: Vec<(&'static str, f64)> = Vec::new();
    for (i, g) in r
        .genres
        .iter()
        .filter_map(|g| taxonomy::canonical(g))
        .enumerate()
        .take(3)
    {
        if !out.iter().any(|(x, _)| *x == g) {
            out.push((g, 1.0 - 0.25 * i as f64));
        }
    }
    Ok(out)
}

/// Artist and title to look up. Plain YouTube uploads are often "Artist - Title (Official Video)"
/// on a channel named after the label; Spotify / YouTube Music metadata is already clean.
pub fn lookup_names(t: &CatalogTrack) -> (String, String) {
    let clean_meta = t
        .info
        .as_ref()
        .is_some_and(|i| matches!(i.origin, TrackOrigin::Spotify | TrackOrigin::YoutubeMusic) || i.music.is_some());
    let mut artist = t.artists.first().map(|a| clean_channel(a)).unwrap_or_default();
    let mut title = strip_noise(&t.title);
    if !clean_meta {
        for sep in [" - ", " – ", " — ", " | "] {
            if let Some((a, rest)) = title.split_once(sep) {
                if !a.trim().is_empty() && !rest.trim().is_empty() {
                    artist = clean_channel(a);
                    title = rest.trim().to_owned();
                    break;
                }
            }
        }
    }
    // "Song ft. Someone" -> "Song"
    let lower = title.to_lowercase();
    for marker in [" ft. ", " ft ", " feat. ", " feat ", " featuring ", " prod. "] {
        if let Some(i) = lower.find(marker) {
            title.truncate(i);
            break;
        }
    }
    (artist.trim().to_owned(), title.trim().to_owned())
}

pub fn clean_channel(a: &str) -> String {
    let mut s = a.trim().to_owned();
    for suffix in [" - Topic", "VEVO", "Vevo", " Official", " Oficial"] {
        if let Some(x) = s.strip_suffix(suffix) {
            s = x.trim().to_owned();
        }
    }
    s
}

/// Drop every bracketed group ("(Official Video)", "[Letra]", "(feat. X)").
fn strip_noise(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut depth = 0usize;
    for ch in title.chars() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_util::track;

    fn cat(title: &str, artist: &str, origin: TrackOrigin) -> CatalogTrack {
        let mut info = track("x");
        info.origin = origin;
        CatalogTrack {
            id: 1,
            key: "x".into(),
            title: title.into(),
            artists: vec![artist.into()],
            url: String::new(),
            duration_ms: None,
            isrc: None,
            tag_status: crate::store::TagStatus::Pending,
            tag_attempts: 0,
            created_at: 0,
            info: Some(info),
        }
    }

    #[test]
    fn lookup_name_cleaning() {
        let t = cat(
            "Bad Bunny - Tití Me Preguntó (Official Video)",
            "Bad Bunny",
            TrackOrigin::Youtube,
        );
        assert_eq!(lookup_names(&t), ("Bad Bunny".into(), "Tití Me Preguntó".into()));
        let t = cat(
            "Despacito ft. Daddy Yankee [Letra]",
            "LuisFonsiVEVO",
            TrackOrigin::Youtube,
        );
        assert_eq!(lookup_names(&t), ("LuisFonsi".into(), "Despacito".into()));
        let t = cat("Love - Remastered", "The Band - Topic", TrackOrigin::YoutubeMusic);
        assert_eq!(lookup_names(&t), ("The Band".into(), "Love - Remastered".into()));
    }

    #[tokio::test]
    async fn no_sources_yields_failed_status() {
        let store = Store::open_in_memory().await.unwrap();
        let id = store.ensure_track(&track("a")).await.unwrap();
        let tagger = Tagger {
            store: store.clone(),
            lastfm: None,
            musicbrainz: None,
            ollama: None,
            artist_ttl: Duration::from_secs(60),
            wake: Arc::new(Notify::new()),
        };
        assert!(!tagger.is_useful());
        let t = store.catalog_track(id).await.unwrap().unwrap();
        let tags = tagger.tag_one(&t, &CancellationToken::new()).await.unwrap();
        assert!(tags.is_empty());
    }
}
