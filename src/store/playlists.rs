//! Saved playlists of each user (Playlists tab): playlists, albums and artist pages they added.
//!
//! At most `max` per user. Saving a new one when the list is full drops the least recently used
//! unpinned entry; pinned entries are never evicted, so when every slot is pinned nothing new is
//! saved ([`SaveOutcome::Full`]). Saving a URL that is already there moves it to the top instead.

use anyhow::Result;
use serde::Serialize;

use super::{now_secs, sf, Store};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SavedPlaylist {
    pub id: i64,
    pub url: String,
    pub platform: String,
    pub name: String,
    pub image_url: Option<String>,
    pub pinned: bool,
    pub added_at: i64,
    pub used_at: i64,
}

/// A list to save, as resolved.
#[derive(Debug, Clone)]
pub struct NewPlaylist<'a> {
    pub url: &'a str,
    pub platform: &'a str,
    pub name: &'a str,
    pub image_url: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Saved (new or moved to the top); `evicted` older unpinned entries were dropped for it.
    Saved { id: i64, evicted: u64 },
    /// Every slot is pinned (or saving is off: `max == 0`). Nothing changed.
    Full,
}

type Row = (i64, String, String, String, Option<String>, bool, i64, i64);

fn from_row((id, url, platform, name, image_url, pinned, added_at, used_at): Row) -> SavedPlaylist {
    SavedPlaylist {
        id,
        url,
        platform,
        name,
        image_url,
        pinned,
        added_at,
        used_at,
    }
}

const COLUMNS: &str = "id, url, platform, name, image_url, pinned, added_at, used_at";

impl Store {
    /// Save (or move to the top) a list for `user_id`, keeping at most `max` entries.
    pub async fn save_playlist(&self, user_id: u64, p: &NewPlaylist<'_>, max: usize) -> Result<SaveOutcome> {
        if max == 0 {
            return Ok(SaveOutcome::Full);
        }
        let uid = sf(user_id);
        let now = now_secs();
        let mut tx = self.begin_write().await?;
        let (seq,): (i64,) =
            sqlx::query_as("SELECT COALESCE(MAX(used_seq), 0) + 1 FROM user_playlists WHERE user_id = ?")
                .bind(uid)
                .fetch_one(&mut *tx)
                .await?;
        // Already saved: refresh its name and image and move it to the top.
        let existing: Option<(i64,)> = sqlx::query_as(
            "UPDATE user_playlists SET name = CASE WHEN ?3 = '' THEN name ELSE ?3 END, \
             image_url = COALESCE(?4, image_url), platform = ?5, used_at = ?6, used_seq = ?7 \
             WHERE user_id = ?1 AND url = ?2 RETURNING id",
        )
        .bind(uid)
        .bind(p.url)
        .bind(p.name)
        .bind(p.image_url)
        .bind(p.platform)
        .bind(now)
        .bind(seq)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((id,)) = existing {
            tx.commit().await?;
            return Ok(SaveOutcome::Saved { id, evicted: 0 });
        }
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM user_playlists WHERE user_id = ?")
            .bind(uid)
            .fetch_one(&mut *tx)
            .await?;
        let mut evicted = 0;
        let over = count + 1 - max as i64;
        if over > 0 {
            // Oldest unpinned first; also trims lists left over a lowered maximum.
            evicted = sqlx::query(
                "DELETE FROM user_playlists WHERE id IN (SELECT id FROM user_playlists \
                 WHERE user_id = ? AND pinned = 0 ORDER BY used_seq ASC LIMIT ?)",
            )
            .bind(uid)
            .bind(over)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if (evicted as i64) < over {
                // Every remaining slot is pinned: keep the list as it was.
                tx.rollback().await?;
                return Ok(SaveOutcome::Full);
            }
        }
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO user_playlists (user_id, url, platform, name, image_url, pinned, added_at, used_at, used_seq) \
             VALUES (?, ?, ?, ?, ?, 0, ?, ?, ?) RETURNING id",
        )
        .bind(uid)
        .bind(p.url)
        .bind(p.platform)
        .bind(p.name)
        .bind(p.image_url)
        .bind(now)
        .bind(now)
        .bind(seq)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(SaveOutcome::Saved { id, evicted })
    }

    /// The user's saved lists: pinned first, then most recently used.
    pub async fn playlists(&self, user_id: u64) -> Result<Vec<SavedPlaylist>> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM user_playlists WHERE user_id = ? ORDER BY pinned DESC, used_seq DESC"
        ))
        .bind(sf(user_id))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(from_row).collect())
    }

    pub async fn playlist(&self, user_id: u64, id: i64) -> Result<Option<SavedPlaylist>> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM user_playlists WHERE user_id = ? AND id = ?"
        ))
        .bind(sf(user_id))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(from_row))
    }

    /// Mark a saved list as just used (queued again): it moves to the top of its group.
    pub async fn touch_playlist(&self, user_id: u64, id: i64) -> Result<bool> {
        let uid = sf(user_id);
        let res = sqlx::query(
            "UPDATE user_playlists SET used_at = ?3, \
             used_seq = (SELECT COALESCE(MAX(used_seq), 0) + 1 FROM user_playlists WHERE user_id = ?1) \
             WHERE user_id = ?1 AND id = ?2",
        )
        .bind(uid)
        .bind(id)
        .bind(now_secs())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// New name / image read from the platform (empty name and `None` keep the stored ones).
    pub async fn update_playlist_meta(&self, user_id: u64, id: i64, name: &str, image_url: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE user_playlists SET name = CASE WHEN ?3 = '' THEN name ELSE ?3 END, \
             image_url = COALESCE(?4, image_url) WHERE user_id = ?1 AND id = ?2",
        )
        .bind(sf(user_id))
        .bind(id)
        .bind(name)
        .bind(image_url)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_playlist_pinned(&self, user_id: u64, id: i64, pinned: bool) -> Result<bool> {
        let res = sqlx::query("UPDATE user_playlists SET pinned = ? WHERE user_id = ? AND id = ?")
            .bind(pinned)
            .bind(sf(user_id))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn remove_playlist(&self, user_id: u64, id: i64) -> Result<bool> {
        let res = sqlx::query("DELETE FROM user_playlists WHERE user_id = ? AND id = ?")
            .bind(sf(user_id))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(url: &str) -> NewPlaylist<'_> {
        NewPlaylist {
            url,
            platform: "spotify",
            name: "Mix",
            image_url: Some("https://i.scdn.co/image/a"),
        }
    }

    async fn urls(store: &Store, user: u64) -> Vec<String> {
        store
            .playlists(user)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.url)
            .collect()
    }

    fn saved_id(o: SaveOutcome) -> i64 {
        match o {
            SaveOutcome::Saved { id, .. } => id,
            SaveOutcome::Full => panic!("expected a save"),
        }
    }

    #[tokio::test]
    async fn newest_first_and_readding_moves_to_the_top() {
        let store = Store::open_in_memory().await.unwrap();
        for u in ["a", "b", "c"] {
            store.save_playlist(1, &list(u), 20).await.unwrap();
        }
        assert_eq!(urls(&store, 1).await, ["c", "b", "a"]);
        let again = NewPlaylist {
            name: "Renamed",
            image_url: None,
            ..list("a")
        };
        let o = store.save_playlist(1, &again, 20).await.unwrap();
        assert!(matches!(o, SaveOutcome::Saved { evicted: 0, .. }));
        assert_eq!(urls(&store, 1).await, ["a", "c", "b"], "no duplicate, moved up");
        let a = &store.playlists(1).await.unwrap()[0];
        assert_eq!(a.name, "Renamed");
        assert_eq!(
            a.image_url.as_deref(),
            Some("https://i.scdn.co/image/a"),
            "a missing image keeps the old one"
        );
        // Per user.
        assert!(store.playlists(2).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn eviction_skips_pinned_and_full_when_all_pinned() {
        let store = Store::open_in_memory().await.unwrap();
        let a = saved_id(store.save_playlist(1, &list("a"), 3).await.unwrap());
        store.save_playlist(1, &list("b"), 3).await.unwrap();
        store.save_playlist(1, &list("c"), 3).await.unwrap();
        assert!(store.set_playlist_pinned(1, a, true).await.unwrap());
        assert_eq!(urls(&store, 1).await, ["a", "c", "b"], "pinned stays on top");

        // Full: the oldest unpinned (b) goes, the pinned oldest (a) stays.
        let o = store.save_playlist(1, &list("d"), 3).await.unwrap();
        assert!(matches!(o, SaveOutcome::Saved { evicted: 1, .. }));
        assert_eq!(urls(&store, 1).await, ["a", "d", "c"]);

        // Pin everything: nothing new is saved and nothing is lost.
        for p in store.playlists(1).await.unwrap() {
            store.set_playlist_pinned(1, p.id, true).await.unwrap();
        }
        assert_eq!(store.save_playlist(1, &list("e"), 3).await.unwrap(), SaveOutcome::Full);
        assert_eq!(store.playlists(1).await.unwrap().len(), 3);
        // Re-adding a saved one still works when full.
        assert!(matches!(
            store.save_playlist(1, &list("c"), 3).await.unwrap(),
            SaveOutcome::Saved { .. }
        ));
        // A lowered maximum trims the unpinned surplus on the next save.
        store.set_playlist_pinned(1, a, false).await.unwrap();
        let o = store.save_playlist(1, &list("f"), 2).await.unwrap();
        assert_eq!(
            o,
            SaveOutcome::Full,
            "a (the only unpinned) is not enough to get down to 2"
        );
        assert_eq!(store.playlists(1).await.unwrap().len(), 3, "rolled back");
        assert_eq!(store.save_playlist(1, &list("g"), 0).await.unwrap(), SaveOutcome::Full);
    }

    #[tokio::test]
    async fn touch_remove_and_ownership() {
        let store = Store::open_in_memory().await.unwrap();
        let a = saved_id(store.save_playlist(1, &list("a"), 20).await.unwrap());
        store.save_playlist(1, &list("b"), 20).await.unwrap();
        assert!(store.touch_playlist(1, a).await.unwrap());
        assert_eq!(urls(&store, 1).await, ["a", "b"]);
        // Another user cannot see, pin, touch or remove it.
        assert!(store.playlist(2, a).await.unwrap().is_none());
        assert!(!store.set_playlist_pinned(2, a, true).await.unwrap());
        assert!(!store.touch_playlist(2, a).await.unwrap());
        assert!(!store.remove_playlist(2, a).await.unwrap());
        store
            .update_playlist_meta(1, a, "New name", Some("https://img/x"))
            .await
            .unwrap();
        let p = store.playlist(1, a).await.unwrap().unwrap();
        assert_eq!(
            (p.name.as_str(), p.image_url.as_deref()),
            ("New name", Some("https://img/x"))
        );
        assert!(store.remove_playlist(1, a).await.unwrap());
        assert_eq!(urls(&store, 1).await, ["b"]);
    }
}
