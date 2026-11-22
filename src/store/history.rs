//! Per-user listening history (distinct tracks, capped) and the privacy wipe.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use super::{
    catalog::{catalog_from_sqlite_row, upsert_track, CatalogTrack, TrackId, CATALOG_COLUMNS, CATALOG_COLUMN_COUNT},
    now_secs, sf, Store,
};
use crate::sources::TrackInfo;

/// Distinct tracks kept per user; the oldest `last_played` is evicted beyond this.
pub const HISTORY_CAP: u32 = 2000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopTrack {
    pub title: String,
    pub artists: Vec<String>,
    pub plays: u32,
    pub url: String,
    pub last_played: i64,
    /// Artwork from the stored TrackInfo (`thumbnail`), unproxied.
    pub artwork_url: Option<String>,
}

/// A history track tagged with one genre (panel taste drill-down).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenreHistoryTrack {
    pub key: String,
    pub title: String,
    pub artists: Vec<String>,
    /// Unproxied; the API rewrites it through the media proxy.
    pub artwork_url: Option<String>,
    pub url: String,
    pub plays: u32,
    pub last_played: i64,
}

/// key, title, artists JSON, thumbnail, url, plays, last played.
type GenreTrackRow = (String, String, String, Option<String>, String, u32, i64);

/// What `forget_history_track` changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetOutcome {
    /// The track left the history and is now disliked.
    Forgotten,
    /// The key is not in the catalog or not in this user's history.
    NotInHistory,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenreWeight {
    pub genre: String,
    pub weight: f64,
}

/// A history entry with its catalog data (candidate generation).
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub track: CatalogTrack,
    pub play_count: u32,
    pub first_played: i64,
    pub last_played: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedUserData {
    pub history: u64,
    pub feedback: u64,
    pub taste: u64,
    pub neighbors: u64,
    /// Saved playlists (Playlists tab).
    pub playlists: u64,
}

impl Store {
    /// Count a play for the user who requested the track. Upserts the catalog and the history row,
    /// bumps the taste version and enforces the cap. Returns None when the user disabled history.
    pub async fn record_play(&self, user_id: u64, info: &TrackInfo) -> Result<Option<TrackId>> {
        self.record_play_at(user_id, info, now_secs()).await
    }

    /// `record_play` with an explicit timestamp (tests and imports).
    pub async fn record_play_at(&self, user_id: u64, info: &TrackInfo, at: i64) -> Result<Option<TrackId>> {
        if !self.history_enabled(user_id).await? {
            return Ok(None);
        }
        let uid = sf(user_id);
        let mut tx = self.begin_write().await?;
        let track_id = upsert_track(&mut tx, info).await?;
        let (plays,): (i64,) = sqlx::query_as(
            "INSERT INTO user_history (user_id, track_id, play_count, first_played, last_played) VALUES (?1, ?2, 1, ?3, ?3) \
             ON CONFLICT(user_id, track_id) DO UPDATE SET play_count = play_count + 1, \
             last_played = MAX(last_played, excluded.last_played) RETURNING play_count",
        )
        .bind(uid)
        .bind(track_id)
        .bind(at)
        .fetch_one(&mut *tx)
        .await?;

        // Only a new distinct track can push the user over the cap.
        if plays == 1 {
            let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM user_history WHERE user_id = ?")
                .bind(uid)
                .fetch_one(&mut *tx)
                .await?;
            let excess = count - HISTORY_CAP as i64;
            if excess > 0 {
                sqlx::query(
                    "DELETE FROM user_history WHERE user_id = ?1 AND track_id IN ( \
                       SELECT track_id FROM user_history WHERE user_id = ?1 AND track_id != ?2 \
                       ORDER BY last_played, track_id LIMIT ?3)",
                )
                .bind(uid)
                .bind(track_id)
                .bind(excess)
                .execute(&mut *tx)
                .await?;
            }
        }

        bump_taste_version(&mut tx, uid).await?;
        tx.commit().await?;
        Ok(Some(track_id))
    }

    /// Number of distinct tracks in a user's history.
    pub async fn history_len(&self, user_id: u64) -> Result<u32> {
        let (n,): (u32,) = sqlx::query_as("SELECT COUNT(*) FROM user_history WHERE user_id = ?")
            .bind(sf(user_id))
            .fetch_one(self.pool())
            .await?;
        Ok(n)
    }

    /// Most played tracks of a user.
    pub async fn top_tracks(&self, user_id: u64, limit: u32) -> Result<Vec<TopTrack>> {
        let rows: Vec<(String, String, u32, String, i64, Option<String>)> = sqlx::query_as(
            "SELECT t.title, t.artists, h.play_count, t.url, h.last_played, json_extract(t.info, '$.thumbnail') \
             FROM user_history h JOIN tracks t ON t.id = h.track_id WHERE h.user_id = ? \
             ORDER BY h.play_count DESC, h.last_played DESC LIMIT ?",
        )
        .bind(sf(user_id))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(title, artists, plays, url, last_played, artwork_url)| TopTrack {
                title,
                artists: serde_json::from_str(&artists).unwrap_or_default(),
                plays,
                url,
                last_played,
                artwork_url: artwork_url.filter(|u| !u.is_empty()),
            })
            .collect())
    }

    /// One page of a user's history tracks tagged with `genre` (any tag weight), most played
    /// first, then most recent, plus the total count. `genre` is a canonical taxonomy name.
    pub async fn history_by_genre(
        &self,
        user_id: u64,
        genre: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<GenreHistoryTrack>, u32)> {
        let uid = sf(user_id);
        let (total,): (u32,) = sqlx::query_as(
            "SELECT COUNT(*) FROM user_history h \
             JOIN track_tags tt ON tt.track_id = h.track_id AND tt.tag = ?2 WHERE h.user_id = ?1",
        )
        .bind(uid)
        .bind(genre)
        .fetch_one(self.pool())
        .await?;
        if limit == 0 || offset >= total {
            return Ok((Vec::new(), total));
        }
        let rows: Vec<GenreTrackRow> = sqlx::query_as(
            "SELECT t.key, t.title, t.artists, json_extract(t.info, '$.thumbnail'), t.url, h.play_count, \
             h.last_played FROM user_history h \
             JOIN track_tags tt ON tt.track_id = h.track_id AND tt.tag = ?2 \
             JOIN tracks t ON t.id = h.track_id WHERE h.user_id = ?1 \
             ORDER BY h.play_count DESC, h.last_played DESC, t.id LIMIT ?3 OFFSET ?4",
        )
        .bind(uid)
        .bind(genre)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool())
        .await?;
        let tracks = rows
            .into_iter()
            .map(
                |(key, title, artists, artwork_url, url, plays, last_played)| GenreHistoryTrack {
                    key,
                    title,
                    artists: serde_json::from_str(&artists).unwrap_or_default(),
                    artwork_url: artwork_url.filter(|u| !u.is_empty()),
                    url,
                    plays,
                    last_played,
                },
            )
            .collect();
        Ok((tracks, total))
    }

    /// Remove one track (by catalog key) from a user's history and dislike it, so the radio never
    /// picks it for them again, then mark their taste stale. One transaction. Callers check that
    /// history is on (like `set_feedback`).
    pub async fn forget_history_track(&self, user_id: u64, key: &str) -> Result<ForgetOutcome> {
        let uid = sf(user_id);
        let mut tx = self.begin_write().await?;
        let removed: Option<(i64,)> = sqlx::query_as(
            "DELETE FROM user_history WHERE user_id = ?1 AND track_id = (SELECT id FROM tracks WHERE key = ?2) \
             RETURNING track_id",
        )
        .bind(uid)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((track_id,)) = removed else {
            return Ok(ForgetOutcome::NotInHistory);
        };
        sqlx::query(
            "INSERT INTO user_feedback (user_id, track_id, value, at) VALUES (?, ?, -1, ?) \
             ON CONFLICT(user_id, track_id) DO UPDATE SET value = excluded.value, at = excluded.at",
        )
        .bind(uid)
        .bind(track_id)
        .bind(now_secs())
        .execute(&mut *tx)
        .await?;
        bump_taste_version(&mut tx, uid).await?;
        tx.commit().await?;
        Ok(ForgetOutcome::Forgotten)
    }

    /// History entries with catalog data, most played first (favorites / taste inputs).
    pub async fn history_entries(&self, user_id: u64, limit: u32) -> Result<Vec<HistoryEntry>> {
        let rows = sqlx::query(&format!(
            "SELECT {CATALOG_COLUMNS}, h.play_count, h.first_played, h.last_played FROM user_history h              JOIN tracks t ON t.id = h.track_id WHERE h.user_id = ?              ORDER BY h.play_count DESC, h.last_played DESC LIMIT ?"
        ))
        .bind(sf(user_id))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|r| {
                let n = CATALOG_COLUMN_COUNT;
                Ok(HistoryEntry {
                    track: catalog_from_sqlite_row(r)?,
                    play_count: r.try_get(n)?,
                    first_played: r.try_get(n + 1)?,
                    last_played: r.try_get(n + 2)?,
                })
            })
            .collect()
    }

    /// Genres of a user's history weighted by play count (raw, without decay or feedback).
    pub async fn top_genres(&self, user_id: u64, limit: u32) -> Result<Vec<GenreWeight>> {
        let rows: Vec<(String, f64)> = sqlx::query_as(
            "SELECT tt.tag, SUM(tt.weight * h.play_count) AS w FROM user_history h \
             JOIN track_tags tt ON tt.track_id = h.track_id WHERE h.user_id = ? \
             GROUP BY tt.tag ORDER BY w DESC, tt.tag LIMIT ?",
        )
        .bind(sf(user_id))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(genre, weight)| GenreWeight { genre, weight })
            .collect())
    }

    /// Delete everything personal about a user: history, feedback, taste and neighbor rows in both
    /// directions. Preferences (locale, history opt-out) are kept.
    pub async fn delete_user_data(&self, user_id: u64) -> Result<DeletedUserData> {
        let uid = sf(user_id);
        let mut tx = self.begin_write().await?;
        let history = sqlx::query("DELETE FROM user_history WHERE user_id = ?")
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        let feedback = sqlx::query("DELETE FROM user_feedback WHERE user_id = ?")
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        let taste = sqlx::query("DELETE FROM user_taste WHERE user_id = ?")
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        let neighbors = sqlx::query("DELETE FROM user_neighbors WHERE user_id = ?1 OR neighbor_id = ?1")
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        let playlists = sqlx::query("DELETE FROM user_playlists WHERE user_id = ?")
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(DeletedUserData {
            history: history.rows_affected(),
            feedback: feedback.rows_affected(),
            taste: taste.rows_affected(),
            neighbors: neighbors.rows_affected(),
            playlists: playlists.rows_affected(),
        })
    }
}

/// Mark the user's taste vector as stale.
pub(crate) async fn bump_taste_version(conn: &mut sqlx::SqliteConnection, uid: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO user_taste (user_id, version) VALUES (?, 1) \
         ON CONFLICT(user_id) DO UPDATE SET version = version + 1",
    )
    .bind(uid)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{test_util::track, Feedback, Tag, TagSource, UserPrefsPatch};

    #[tokio::test]
    async fn play_counts_without_duplicates() {
        let store = Store::open_in_memory().await.unwrap();
        let a = store.record_play_at(1, &track("a"), 100).await.unwrap().unwrap();
        store.record_play_at(1, &track("a"), 200).await.unwrap();
        store.record_play_at(1, &track("b"), 150).await.unwrap();
        store.record_play_at(2, &track("a"), 150).await.unwrap();
        assert_eq!(store.history_len(1).await.unwrap(), 2);

        let top = store.top_tracks(1, 10).await.unwrap();
        assert_eq!(top[0].title, "Song a");
        assert_eq!(top[0].plays, 2);
        assert_eq!(top[0].last_played, 200);

        let entries = store.history_entries(1, 10).await.unwrap();
        assert_eq!(entries[0].track.id, a);
        assert_eq!(entries[0].first_played, 100);
        assert_eq!(entries[0].track.info.as_ref().unwrap().youtube_id.as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn cap_evicts_oldest_last_played() {
        let store = Store::open_in_memory().await.unwrap();
        let n = HISTORY_CAP as i64 + 5;
        for i in 0..n {
            store
                .record_play_at(1, &track(&format!("t{i}")), 1000 + i)
                .await
                .unwrap();
        }
        assert_eq!(store.history_len(1).await.unwrap(), HISTORY_CAP);
        // t0..t4 were the oldest and got evicted; replaying t5 makes it the newest.
        let keys: Vec<(String,)> = sqlx::query_as(
            "SELECT t.key FROM user_history h JOIN tracks t ON t.id = h.track_id WHERE h.user_id = 1 \
             ORDER BY h.last_played LIMIT 1",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(keys[0].0, "t5");
        store.record_play_at(1, &track("t5"), 99_999).await.unwrap();
        store.record_play_at(1, &track("new"), 100_000).await.unwrap();
        assert_eq!(store.history_len(1).await.unwrap(), HISTORY_CAP);
        let tops = store.history_entries(1, HISTORY_CAP).await.unwrap();
        assert!(tops.iter().any(|e| e.track.key == "t5"), "replayed track survives");
        assert!(!tops.iter().any(|e| e.track.key == "t6"), "next oldest evicted");
        // Catalog keeps evicted tracks.
        assert!(store.track_id("t0").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn respects_history_opt_out() {
        let store = Store::open_in_memory().await.unwrap();
        store
            .update_user_prefs(
                1,
                &UserPrefsPatch {
                    history_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(store.record_play(1, &track("a")).await.unwrap().is_none());
        assert_eq!(store.history_len(1).await.unwrap(), 0);
        assert!(store.track_id("a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn top_genres_weighted() {
        let store = Store::open_in_memory().await.unwrap();
        let a = store.record_play_at(1, &track("a"), 1).await.unwrap().unwrap();
        store.record_play_at(1, &track("a"), 2).await.unwrap();
        let b = store.record_play_at(1, &track("b"), 3).await.unwrap().unwrap();
        let t = |tag: &str, weight| Tag {
            tag: tag.into(),
            weight,
            source: TagSource::Lastfm,
        };
        store.write_track_tags(a, &[t("rock", 1.0)]).await.unwrap();
        store
            .write_track_tags(b, &[t("jazz", 1.0), t("rock", 0.5)])
            .await
            .unwrap();
        let g = store.top_genres(1, 10).await.unwrap();
        assert_eq!(
            g[0],
            GenreWeight {
                genre: "rock".into(),
                weight: 2.5
            }
        );
        assert_eq!(g[1].genre, "jazz");
    }

    #[tokio::test]
    async fn history_by_genre_filters_orders_and_pages() {
        let store = Store::open_in_memory().await.unwrap();
        let t = |tag: &str| Tag {
            tag: tag.into(),
            weight: 0.4,
            source: TagSource::Lastfm,
        };
        let mut art = track("a");
        art.thumbnail = Some("https://i.ytimg.com/vi/a/hq.jpg".into());
        // a: 2 plays, b: 1 play (older), c: 1 play (newer), d: jazz only, e: rock but only user 2.
        let a = store.record_play_at(1, &art, 10).await.unwrap().unwrap();
        store.record_play_at(1, &art, 20).await.unwrap();
        let b = store.record_play_at(1, &track("b"), 5).await.unwrap().unwrap();
        let c = store.record_play_at(1, &track("c"), 30).await.unwrap().unwrap();
        let d = store.record_play_at(1, &track("d"), 40).await.unwrap().unwrap();
        let e = store.record_play_at(2, &track("e"), 50).await.unwrap().unwrap();
        for id in [a, b, c, e] {
            store.write_track_tags(id, &[t("rock")]).await.unwrap();
        }
        store.write_track_tags(d, &[t("jazz")]).await.unwrap();

        let (all, total) = store.history_by_genre(1, "rock", 50, 0).await.unwrap();
        assert_eq!(total, 3);
        let keys: Vec<&str> = all.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(keys, ["a", "c", "b"], "plays, then recency; only the caller's history");
        assert_eq!(all[0].plays, 2);
        assert_eq!(all[0].last_played, 20);
        assert_eq!(all[0].artwork_url.as_deref(), Some("https://i.ytimg.com/vi/a/hq.jpg"));
        assert_eq!(all[1].artwork_url, None);
        assert_eq!(all[1].title, "Song c");
        assert_eq!(all[1].artists, ["Artist c"]);

        let (page, total) = store.history_by_genre(1, "rock", 1, 1).await.unwrap();
        assert_eq!((page.len(), page[0].key.as_str(), total), (1, "c", 3));
        let (past_end, total) = store.history_by_genre(1, "rock", 10, 3).await.unwrap();
        assert!(past_end.is_empty());
        assert_eq!(total, 3);
        let (jazz, _) = store.history_by_genre(1, "jazz", 10, 0).await.unwrap();
        assert_eq!(jazz.len(), 1);
        assert_eq!(store.history_by_genre(2, "jazz", 10, 0).await.unwrap().1, 0);

        let top = store.top_tracks(1, 1).await.unwrap();
        assert_eq!(top[0].artwork_url.as_deref(), Some("https://i.ytimg.com/vi/a/hq.jpg"));
    }

    #[tokio::test]
    async fn forget_history_track_dislikes_and_marks_taste_stale() {
        let store = Store::open_in_memory().await.unwrap();
        store.record_play(1, &track("a")).await.unwrap();
        store.record_play(1, &track("b")).await.unwrap();
        store.record_play(2, &track("a")).await.unwrap();
        store.set_feedback(1, &track("a"), Some(Feedback::Like)).await.unwrap();
        let stale = store.stale_tastes(10).await.unwrap();
        for s in &stale {
            store
                .put_taste(s.user_id, &[("rock".into(), 1.0)].into_iter().collect(), s.version)
                .await
                .unwrap();
        }
        assert!(store.stale_tastes(10).await.unwrap().is_empty());

        assert_eq!(
            store.forget_history_track(1, "a").await.unwrap(),
            ForgetOutcome::Forgotten
        );
        assert_eq!(store.history_len(1).await.unwrap(), 1);
        assert_eq!(
            store.get_feedback(1, &track("a")).await.unwrap(),
            Some(Feedback::Dislike),
            "like flips to dislike"
        );
        let stale = store.stale_tastes(10).await.unwrap();
        assert_eq!(stale.iter().map(|s| s.user_id).collect::<Vec<_>>(), [1]);
        // Other users untouched.
        assert_eq!(store.history_len(2).await.unwrap(), 1);
        assert!(store.get_feedback(2, &track("a")).await.unwrap().is_none());
        // The radio filter now excludes it for user 1.
        let a = store.track_id("a").await.unwrap().unwrap();
        assert!(store.disliked_tracks(&[1]).await.unwrap().contains(&a));

        // Again, unknown keys and other users' tracks are not found and change nothing.
        assert_eq!(
            store.forget_history_track(1, "a").await.unwrap(),
            ForgetOutcome::NotInHistory
        );
        assert_eq!(
            store.forget_history_track(1, "nope").await.unwrap(),
            ForgetOutcome::NotInHistory
        );
        store.record_play(3, &track("z")).await.unwrap();
        assert_eq!(
            store.forget_history_track(1, "z").await.unwrap(),
            ForgetOutcome::NotInHistory
        );
        assert!(store.get_feedback(1, &track("z")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_user_data_wipes_everything_personal() {
        let store = Store::open_in_memory().await.unwrap();
        store.record_play(1, &track("a")).await.unwrap();
        store.record_play(2, &track("a")).await.unwrap();
        store.set_feedback(1, &track("a"), Some(Feedback::Like)).await.unwrap();
        store
            .put_taste(1, &[("rock".into(), 1.0)].into_iter().collect(), 1)
            .await
            .unwrap();
        store.replace_neighbors(1, &[(2, 0.9)]).await.unwrap();
        store.replace_neighbors(2, &[(1, 0.9)]).await.unwrap();
        let saved = |url| crate::store::NewPlaylist {
            url,
            platform: "youtube",
            name: "List",
            image_url: None,
        };
        store.save_playlist(1, &saved("https://www.youtube.com/playlist?list=PL1"), 20).await.unwrap();
        store.save_playlist(2, &saved("https://www.youtube.com/playlist?list=PL1"), 20).await.unwrap();
        store
            .update_user_prefs(
                1,
                &UserPrefsPatch {
                    locale: Some(Some("es".into())),
                    history_enabled: None,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let d = store.delete_user_data(1).await.unwrap();
        assert_eq!(
            d,
            DeletedUserData {
                history: 1,
                feedback: 1,
                taste: 1,
                neighbors: 2,
                playlists: 1,
            }
        );
        assert_eq!(store.history_len(1).await.unwrap(), 0);
        assert!(store.get_feedback(1, &track("a")).await.unwrap().is_none());
        assert!(store.get_taste(1).await.unwrap().is_none());
        assert!(store.neighbors(2, 10).await.unwrap().is_empty());
        // Other users and prefs untouched.
        assert_eq!(store.history_len(2).await.unwrap(), 1);
        assert!(store.playlists(1).await.unwrap().is_empty());
        assert_eq!(store.playlists(2).await.unwrap().len(), 1);
        assert_eq!(store.user_prefs(1).await.unwrap().locale.as_deref(), Some("es"));
    }
}
