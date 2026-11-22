//! Global track catalog and genre tags (track and artist level).

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqliteRow, QueryBuilder, Row, Sqlite, SqliteConnection};

use super::{now_secs, Store};
use crate::sources::{TrackInfo, TrackOrigin};

/// Row id in `tracks`.
pub type TrackId = i64;

/// Canonical catalog key: YouTube id, else `isrc:<ISRC>`, else `spotify:<id>`, else `url:<url>`.
pub fn track_key(info: &TrackInfo) -> String {
    if let Some(id) = info.youtube_id.as_deref().filter(|s| !s.is_empty()) {
        id.to_owned()
    } else if let Some(isrc) = info.isrc.as_deref().filter(|s| !s.is_empty()) {
        format!("isrc:{}", isrc.to_ascii_uppercase())
    } else if let Some(id) = info.spotify_id.as_deref().filter(|s| !s.is_empty()) {
        format!("spotify:{id}")
    } else {
        format!("url:{}", info.url)
    }
}

/// Lowercase, trimmed, single-spaced artist name used as the `artist_tags` key.
pub fn normalize_artist(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagStatus {
    Pending,
    Done,
    Failed,
}

impl TagStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "done" => Self::Done,
            "failed" => Self::Failed,
            _ => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagSource {
    Lastfm,
    Musicbrainz,
    Ollama,
}

impl TagSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lastfm => "lastfm",
            Self::Musicbrainz => "musicbrainz",
            Self::Ollama => "ollama",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "musicbrainz" => Self::Musicbrainz,
            "ollama" => Self::Ollama,
            _ => Self::Lastfm,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    pub tag: String,
    pub weight: f64,
    pub source: TagSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogTrack {
    pub id: TrackId,
    pub key: String,
    pub title: String,
    pub artists: Vec<String>,
    pub url: String,
    pub duration_ms: Option<u64>,
    pub isrc: Option<String>,
    pub tag_status: TagStatus,
    pub tag_attempts: u32,
    pub created_at: i64,
    /// Latest full TrackInfo, to put the track back in a queue.
    #[serde(skip)]
    pub info: Option<TrackInfo>,
}

pub(crate) const CATALOG_COLUMNS: &str =
    "t.id, t.key, t.title, t.artists, t.url, t.duration_ms, t.isrc, t.tag_status, t.tag_attempts, t.created_at, t.info";

pub(crate) type CatalogRow =
    (i64, String, String, String, String, Option<i64>, Option<String>, String, u32, i64, String);

pub(crate) fn catalog_from_row(r: CatalogRow) -> CatalogTrack {
    CatalogTrack {
        id: r.0,
        key: r.1,
        title: r.2,
        artists: serde_json::from_str(&r.3).unwrap_or_default(),
        url: r.4,
        duration_ms: r.5.map(|d| d.max(0) as u64),
        isrc: r.6,
        tag_status: TagStatus::parse(&r.7),
        tag_attempts: r.8,
        created_at: r.9,
        info: serde_json::from_str(&r.10).ok(),
    }
}

/// Number of columns in `CATALOG_COLUMNS`; extra selected columns start at this index.
pub(crate) const CATALOG_COLUMN_COUNT: usize = 11;

/// Decode the leading `CATALOG_COLUMNS` of a row that selects more columns after them.
pub(crate) fn catalog_from_sqlite_row(row: &SqliteRow) -> Result<CatalogTrack> {
    Ok(catalog_from_row((
        row.try_get(0)?,
        row.try_get(1)?,
        row.try_get(2)?,
        row.try_get(3)?,
        row.try_get(4)?,
        row.try_get(5)?,
        row.try_get(6)?,
        row.try_get(7)?,
        row.try_get(8)?,
        row.try_get(9)?,
        row.try_get(10)?,
    )))
}

/// Insert or refresh a catalog entry; returns its id. Title/artists are only overwritten by
/// sources with clean metadata (Spotify, YouTube Music), so a later raw YouTube title never
/// replaces a clean one.
pub(crate) async fn upsert_track(conn: &mut SqliteConnection, info: &TrackInfo) -> Result<TrackId> {
    let clean = matches!(info.origin, TrackOrigin::Spotify | TrackOrigin::YoutubeMusic);
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO tracks (key, title, artists, url, duration_ms, isrc, info, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT(key) DO UPDATE SET \
           title = CASE WHEN ?9 THEN excluded.title ELSE title END, \
           artists = CASE WHEN ?9 THEN excluded.artists ELSE artists END, \
           url = excluded.url, \
           duration_ms = COALESCE(excluded.duration_ms, duration_ms), \
           isrc = COALESCE(excluded.isrc, isrc), \
           info = excluded.info \
         RETURNING id",
    )
    .bind(track_key(info))
    .bind(&info.title)
    .bind(serde_json::to_string(&info.artists)?)
    .bind(&info.url)
    .bind(info.duration_ms.map(|d| d as i64))
    .bind(info.isrc.as_deref().map(str::to_ascii_uppercase))
    .bind(serde_json::to_string(info)?)
    .bind(now_secs())
    .bind(clean)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

pub(crate) async fn lookup_track_id(conn: &mut SqliteConnection, key: &str) -> Result<Option<TrackId>> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM tracks WHERE key = ?").bind(key).fetch_optional(&mut *conn).await?;
    Ok(row.map(|r| r.0))
}

impl Store {
    /// Make sure a track is in the catalog (queued for tagging when new). Returns its id.
    pub async fn ensure_track(&self, info: &TrackInfo) -> Result<TrackId> {
        let mut conn = self.pool().acquire().await?;
        upsert_track(&mut conn, info).await
    }

    pub async fn track_id(&self, key: &str) -> Result<Option<TrackId>> {
        let mut conn = self.pool().acquire().await?;
        lookup_track_id(&mut conn, key).await
    }

    pub async fn catalog_track(&self, id: TrackId) -> Result<Option<CatalogTrack>> {
        let row: Option<CatalogRow> = sqlx::query_as(&format!("SELECT {CATALOG_COLUMNS} FROM tracks t WHERE t.id = ?"))
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(catalog_from_row))
    }

    /// Next tracks waiting for genre tags, least-tried and oldest first.
    pub async fn pending_tag_tracks(&self, limit: u32) -> Result<Vec<CatalogTrack>> {
        let rows: Vec<CatalogRow> = sqlx::query_as(&format!(
            "SELECT {CATALOG_COLUMNS} FROM tracks t WHERE t.tag_status = 'pending' \
             ORDER BY t.tag_attempts, t.created_at LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(catalog_from_row).collect())
    }

    /// Replace every tag of a track and mark it done (or failed when `tags` is empty).
    pub async fn write_track_tags(&self, track_id: TrackId, tags: &[Tag]) -> Result<()> {
        let mut tx = self.begin_write().await?;
        sqlx::query("DELETE FROM track_tags WHERE track_id = ?").bind(track_id).execute(&mut *tx).await?;
        if !tags.is_empty() {
            let mut qb: QueryBuilder<Sqlite> =
                QueryBuilder::new("INSERT INTO track_tags (track_id, tag, weight, source) ");
            qb.push_values(tags, |mut b, t| {
                b.push_bind(track_id).push_bind(t.tag.clone()).push_bind(t.weight).push_bind(t.source.as_str());
            });
            qb.push(" ON CONFLICT(track_id, tag) DO UPDATE SET weight = MAX(weight, excluded.weight)");
            qb.build().execute(&mut *tx).await?;
        }
        let status = if tags.is_empty() { TagStatus::Failed } else { TagStatus::Done };
        sqlx::query("UPDATE tracks SET tag_status = ?, tagged_at = ?, tag_attempts = tag_attempts + 1 WHERE id = ?")
            .bind(status.as_str())
            .bind(now_secs())
            .bind(track_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// A transient tagging error: count the attempt and give up after `max_attempts`.
    pub async fn record_tag_failure(&self, track_id: TrackId, max_attempts: u32) -> Result<TagStatus> {
        let (status,): (String,) = sqlx::query_as(
            "UPDATE tracks SET tag_attempts = tag_attempts + 1, \
             tag_status = CASE WHEN tag_attempts + 1 >= ? THEN 'failed' ELSE tag_status END \
             WHERE id = ? RETURNING tag_status",
        )
        .bind(max_attempts)
        .bind(track_id)
        .fetch_one(self.pool())
        .await?;
        Ok(TagStatus::parse(&status))
    }

    /// Tags of a track, heaviest first.
    pub async fn track_tags(&self, track_id: TrackId) -> Result<Vec<Tag>> {
        let rows: Vec<(String, f64, String)> =
            sqlx::query_as("SELECT tag, weight, source FROM track_tags WHERE track_id = ? ORDER BY weight DESC, tag")
                .bind(track_id)
                .fetch_all(self.pool())
                .await?;
        Ok(rows
            .into_iter()
            .map(|(tag, weight, source)| Tag { tag, weight, source: TagSource::parse(&source) })
            .collect())
    }

    /// Genre names for a track by catalog key (now-playing display). Empty when unknown.
    pub async fn track_genres_by_key(&self, key: &str, limit: u32) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT tt.tag FROM tracks t JOIN track_tags tt ON tt.track_id = t.id WHERE t.key = ? \
             ORDER BY tt.weight DESC, tt.tag LIMIT ?",
        )
        .bind(key)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// Cached artist tags, or None when absent or older than `max_age`.
    pub async fn artist_tags(&self, artist: &str, max_age: Duration) -> Result<Option<Vec<Tag>>> {
        let rows: Vec<(String, f64, String)> = sqlx::query_as(
            "SELECT tag, weight, source FROM artist_tags WHERE artist_norm = ? AND fetched_at > ? \
             ORDER BY weight DESC, tag",
        )
        .bind(normalize_artist(artist))
        .bind(now_secs() - max_age.as_secs() as i64)
        .fetch_all(self.pool())
        .await?;
        Ok((!rows.is_empty()).then(|| {
            rows.into_iter()
                .map(|(tag, weight, source)| Tag { tag, weight, source: TagSource::parse(&source) })
                .collect()
        }))
    }

    /// Replace the cached tags of an artist.
    pub async fn put_artist_tags(&self, artist: &str, tags: &[Tag]) -> Result<()> {
        let norm = normalize_artist(artist);
        let now = now_secs();
        let mut tx = self.begin_write().await?;
        sqlx::query("DELETE FROM artist_tags WHERE artist_norm = ?").bind(&norm).execute(&mut *tx).await?;
        if !tags.is_empty() {
            let mut qb: QueryBuilder<Sqlite> =
                QueryBuilder::new("INSERT INTO artist_tags (artist_norm, tag, weight, source, fetched_at) ");
            qb.push_values(tags, |mut b, t| {
                b.push_bind(norm.clone())
                    .push_bind(t.tag.clone())
                    .push_bind(t.weight)
                    .push_bind(t.source.as_str())
                    .push_bind(now);
            });
            qb.push(" ON CONFLICT(artist_norm, tag) DO UPDATE SET weight = MAX(weight, excluded.weight)");
            qb.build().execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Drop artist tags fetched before `max_age` ago; returns how many rows were removed.
    pub async fn sweep_artist_tags(&self, max_age: Duration) -> Result<u64> {
        let res = sqlx::query("DELETE FROM artist_tags WHERE fetched_at <= ?")
            .bind(now_secs() - max_age.as_secs() as i64)
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_util::track;

    fn tag(t: &str, w: f64) -> Tag {
        Tag { tag: t.into(), weight: w, source: TagSource::Lastfm }
    }

    #[test]
    fn keys() {
        let mut t = track("abc");
        assert_eq!(track_key(&t), "abc");
        t.youtube_id = None;
        t.isrc = Some("usrc17607839".into());
        assert_eq!(track_key(&t), "isrc:USRC17607839");
        t.isrc = None;
        t.spotify_id = Some("sp1".into());
        assert_eq!(track_key(&t), "spotify:sp1");
        assert_eq!(normalize_artist("  The   Weeknd "), "the weeknd");
    }

    #[tokio::test]
    async fn upsert_is_idempotent_and_keeps_clean_title() {
        let store = Store::open_in_memory().await.unwrap();
        let mut clean = track("x1");
        clean.origin = TrackOrigin::YoutubeMusic;
        clean.title = "Clean".into();
        let id = store.ensure_track(&clean).await.unwrap();
        let mut raw = track("x1");
        raw.title = "Clean (Official Video)".into();
        raw.isrc = Some("ABC".into());
        assert_eq!(store.ensure_track(&raw).await.unwrap(), id);
        let t = store.catalog_track(id).await.unwrap().unwrap();
        assert_eq!(t.title, "Clean");
        assert_eq!(t.isrc.as_deref(), Some("ABC"));
        assert_eq!(t.tag_status, TagStatus::Pending);
    }

    #[tokio::test]
    async fn tag_queue() {
        let store = Store::open_in_memory().await.unwrap();
        let a = store.ensure_track(&track("a")).await.unwrap();
        let b = store.ensure_track(&track("b")).await.unwrap();
        let c = store.ensure_track(&track("c")).await.unwrap();
        assert_eq!(store.pending_tag_tracks(10).await.unwrap().len(), 3);

        store.write_track_tags(a, &[tag("rock", 1.0), tag("indie", 0.5)]).await.unwrap();
        store.write_track_tags(b, &[]).await.unwrap();
        assert_eq!(store.record_tag_failure(c, 3).await.unwrap(), TagStatus::Pending);
        let pending = store.pending_tag_tracks(10).await.unwrap();
        assert_eq!(pending.iter().map(|t| t.id).collect::<Vec<_>>(), vec![c]);
        assert_eq!(store.record_tag_failure(c, 3).await.unwrap(), TagStatus::Pending);
        assert_eq!(store.record_tag_failure(c, 3).await.unwrap(), TagStatus::Failed);
        assert!(store.pending_tag_tracks(10).await.unwrap().is_empty());

        assert_eq!(store.catalog_track(b).await.unwrap().unwrap().tag_status, TagStatus::Failed);
        assert_eq!(store.track_tags(a).await.unwrap()[0].tag, "rock");
        assert_eq!(store.track_genres_by_key("a", 5).await.unwrap(), vec!["rock", "indie"]);

        // Rewriting replaces.
        store.write_track_tags(a, &[tag("pop", 1.0)]).await.unwrap();
        assert_eq!(store.track_genres_by_key("a", 5).await.unwrap(), vec!["pop"]);
    }

    #[tokio::test]
    async fn artist_tag_cache() {
        let store = Store::open_in_memory().await.unwrap();
        let day = Duration::from_secs(86_400);
        assert!(store.artist_tags("Foo", day).await.unwrap().is_none());
        store.put_artist_tags("Foo", &[tag("jazz", 1.0)]).await.unwrap();
        assert_eq!(store.artist_tags(" foo ", day).await.unwrap().unwrap()[0].tag, "jazz");
        assert!(store.artist_tags("foo", Duration::ZERO).await.unwrap().is_none());
        assert_eq!(store.sweep_artist_tags(day).await.unwrap(), 0);
        assert_eq!(store.sweep_artist_tags(Duration::ZERO).await.unwrap(), 1);
    }
}
