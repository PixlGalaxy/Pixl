//! Taste vectors and similar-user lists, recomputed by a background job.

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, Sqlite};

use super::{
    catalog::{catalog_from_sqlite_row, CatalogTrack, CATALOG_COLUMNS, CATALOG_COLUMN_COUNT},
    now_secs, sf, snowflake, Store,
};

/// Genre -> weight.
pub type TasteVector = HashMap<String, f64>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserTaste {
    #[serde(with = "snowflake")]
    pub user_id: u64,
    pub vector: TasteVector,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Neighbor {
    #[serde(with = "snowflake")]
    pub user_id: u64,
    pub score: f64,
}

/// A user whose taste changed since it was last computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleTaste {
    pub user_id: u64,
    /// Pass back to `put_taste` so changes made during the computation keep the row stale.
    pub version: i64,
}

/// One (track, tag) row of a user's history, input for the taste computation.
#[derive(Debug, Clone, PartialEq)]
pub struct TasteInput {
    pub tag: String,
    pub tag_weight: f64,
    pub play_count: u32,
    pub last_played: i64,
    /// +1 like, -1 dislike, 0 none.
    pub feedback: i64,
}

/// Neighbor-heard track the user never played (discover candidates).
#[derive(Debug, Clone)]
pub struct DiscoverTrack {
    pub track: CatalogTrack,
    pub score: f64,
}

impl Store {
    pub async fn get_taste(&self, user_id: u64) -> Result<Option<UserTaste>> {
        let row: Option<(Option<String>, i64)> =
            sqlx::query_as("SELECT vector, updated_at FROM user_taste WHERE user_id = ?")
                .bind(sf(user_id))
                .fetch_optional(self.pool())
                .await?;
        Ok(row.and_then(|(vector, updated_at)| {
            let vector = serde_json::from_str(vector.as_deref()?).ok()?;
            Some(UserTaste { user_id, vector, updated_at })
        }))
    }

    /// Store a computed vector. `version` is the one read from `stale_tastes` (0 if unknown).
    pub async fn put_taste(&self, user_id: u64, vector: &TasteVector, version: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_taste (user_id, vector, version, computed_version, updated_at) VALUES (?1, ?2, ?3, ?3, ?4) \
             ON CONFLICT(user_id) DO UPDATE SET vector = excluded.vector, \
             computed_version = MAX(computed_version, excluded.computed_version), updated_at = excluded.updated_at",
        )
        .bind(sf(user_id))
        .bind(serde_json::to_string(vector)?)
        .bind(version)
        .bind(now_secs())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Users whose taste needs recomputing (new plays or feedback since the last run).
    pub async fn stale_tastes(&self, limit: u32) -> Result<Vec<StaleTaste>> {
        let rows: Vec<(u64, i64)> = sqlx::query_as(
            "SELECT user_id, version FROM user_taste WHERE version > computed_version ORDER BY user_id LIMIT ?",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|(user_id, version)| StaleTaste { user_id, version }).collect())
    }

    /// Page through computed taste vectors by user id (similarity job). Users with history
    /// disabled are skipped.
    pub async fn taste_page(&self, after_user_id: u64, limit: u32) -> Result<Vec<UserTaste>> {
        let rows: Vec<(u64, String, i64)> = sqlx::query_as(
            "SELECT t.user_id, t.vector, t.updated_at FROM user_taste t \
             LEFT JOIN user_prefs p ON p.user_id = t.user_id \
             WHERE t.user_id > ? AND t.vector IS NOT NULL AND COALESCE(p.history_enabled, 1) = 1 \
             ORDER BY t.user_id LIMIT ?",
        )
        .bind(sf(after_user_id))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(user_id, v, updated_at)| {
                Some(UserTaste { user_id, vector: serde_json::from_str(&v).ok()?, updated_at })
            })
            .collect())
    }

    /// Per (track, tag) rows of a user's history plus their feedback, for the taste job.
    /// Liked tracks outside the history are included with play_count 0.
    pub async fn taste_inputs(&self, user_id: u64) -> Result<Vec<TasteInput>> {
        let rows: Vec<(String, f64, u32, i64, i64)> = sqlx::query_as(
            "SELECT tt.tag, tt.weight, h.play_count, h.last_played, COALESCE(f.value, 0) FROM user_history h \
             JOIN track_tags tt ON tt.track_id = h.track_id \
             LEFT JOIN user_feedback f ON f.user_id = h.user_id AND f.track_id = h.track_id \
             WHERE h.user_id = ?1 \
             UNION ALL \
             SELECT tt.tag, tt.weight, 0, f.at, f.value FROM user_feedback f \
             JOIN track_tags tt ON tt.track_id = f.track_id \
             WHERE f.user_id = ?1 AND NOT EXISTS ( \
               SELECT 1 FROM user_history h WHERE h.user_id = f.user_id AND h.track_id = f.track_id)",
        )
        .bind(sf(user_id))
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(tag, tag_weight, play_count, last_played, feedback)| TasteInput {
                tag,
                tag_weight,
                play_count,
                last_played,
                feedback,
            })
            .collect())
    }

    /// Replace a user's top-K neighbors.
    pub async fn replace_neighbors(&self, user_id: u64, neighbors: &[(u64, f64)]) -> Result<()> {
        let uid = sf(user_id);
        let mut tx = self.begin_write().await?;
        sqlx::query("DELETE FROM user_neighbors WHERE user_id = ?").bind(uid).execute(&mut *tx).await?;
        let rows: Vec<_> = neighbors.iter().filter(|(n, _)| *n != user_id).collect();
        for chunk in rows.chunks(300) {
            let mut qb: QueryBuilder<Sqlite> =
                QueryBuilder::new("INSERT INTO user_neighbors (user_id, neighbor_id, score) ");
            qb.push_values(chunk, |mut b, (n, score)| {
                b.push_bind(uid).push_bind(sf(*n)).push_bind(*score);
            });
            qb.push(" ON CONFLICT(user_id, neighbor_id) DO UPDATE SET score = excluded.score");
            qb.build().execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Best neighbors, skipping users that disabled history since the list was computed.
    pub async fn neighbors(&self, user_id: u64, limit: u32) -> Result<Vec<Neighbor>> {
        let rows: Vec<(u64, f64)> = sqlx::query_as(
            "SELECT n.neighbor_id, n.score FROM user_neighbors n \
             LEFT JOIN user_prefs p ON p.user_id = n.neighbor_id \
             WHERE n.user_id = ? AND COALESCE(p.history_enabled, 1) = 1 \
             ORDER BY n.score DESC LIMIT ?",
        )
        .bind(sf(user_id))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|(user_id, score)| Neighbor { user_id, score }).collect())
    }

    /// Tracks popular with the user's neighbors that the user never played nor disliked,
    /// scored by sum(neighbor score * neighbor plays).
    pub async fn discover_tracks(&self, user_id: u64, limit: u32) -> Result<Vec<DiscoverTrack>> {
        let rows = sqlx::query(&format!(
            "SELECT {CATALOG_COLUMNS}, SUM(n.score * h.play_count) AS s FROM user_neighbors n              LEFT JOIN user_prefs p ON p.user_id = n.neighbor_id              JOIN user_history h ON h.user_id = n.neighbor_id              JOIN tracks t ON t.id = h.track_id              WHERE n.user_id = ?1 AND COALESCE(p.history_enabled, 1) = 1                AND NOT EXISTS (SELECT 1 FROM user_history x WHERE x.user_id = ?1 AND x.track_id = h.track_id)                AND NOT EXISTS (SELECT 1 FROM user_feedback f WHERE f.user_id = ?1 AND f.track_id = h.track_id AND f.value < 0)              GROUP BY t.id ORDER BY s DESC LIMIT ?2"
        ))
        .bind(sf(user_id))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|r| Ok(DiscoverTrack { track: catalog_from_sqlite_row(r)?, score: r.try_get(CATALOG_COLUMN_COUNT)? }))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{test_util::track, Feedback, Tag, TagSource, UserPrefsPatch};

    #[tokio::test]
    async fn stale_tracking_with_versions() {
        let store = Store::open_in_memory().await.unwrap();
        store.record_play(1, &track("a")).await.unwrap();
        store.record_play(1, &track("b")).await.unwrap();
        let stale = store.stale_tastes(10).await.unwrap();
        assert_eq!(stale, vec![StaleTaste { user_id: 1, version: 2 }]);

        // A play lands while the job computes: the row stays stale.
        store.record_play(1, &track("c")).await.unwrap();
        let v: TasteVector = [("rock".to_string(), 1.0)].into_iter().collect();
        store.put_taste(1, &v, stale[0].version).await.unwrap();
        assert_eq!(store.stale_tastes(10).await.unwrap()[0].version, 3);
        store.put_taste(1, &v, 3).await.unwrap();
        assert!(store.stale_tastes(10).await.unwrap().is_empty());
        assert_eq!(store.get_taste(1).await.unwrap().unwrap().vector["rock"], 1.0);
        assert_eq!(store.taste_page(0, 10).await.unwrap().len(), 1);

        store.set_feedback(1, &track("a"), Some(Feedback::Like)).await.unwrap();
        assert_eq!(store.stale_tastes(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn taste_inputs_include_likes() {
        let store = Store::open_in_memory().await.unwrap();
        let a = store.record_play(1, &track("a")).await.unwrap().unwrap();
        let b = store.ensure_track(&track("b")).await.unwrap();
        let t = |tag: &str| Tag { tag: tag.into(), weight: 1.0, source: TagSource::Lastfm };
        store.write_track_tags(a, &[t("rock")]).await.unwrap();
        store.write_track_tags(b, &[t("jazz")]).await.unwrap();
        store.set_feedback(1, &track("a"), Some(Feedback::Dislike)).await.unwrap();
        store.set_feedback(1, &track("b"), Some(Feedback::Like)).await.unwrap();
        let mut inputs = store.taste_inputs(1).await.unwrap();
        inputs.sort_by(|x, y| x.tag.cmp(&y.tag));
        assert_eq!(inputs.len(), 2);
        assert_eq!((inputs[0].tag.as_str(), inputs[0].play_count, inputs[0].feedback), ("jazz", 0, 1));
        assert_eq!((inputs[1].tag.as_str(), inputs[1].play_count, inputs[1].feedback), ("rock", 1, -1));
    }

    #[tokio::test]
    async fn neighbors_and_discover() {
        let store = Store::open_in_memory().await.unwrap();
        store.record_play(1, &track("mine")).await.unwrap();
        for _ in 0..3 {
            store.record_play(2, &track("popular")).await.unwrap();
        }
        store.record_play(2, &track("mine")).await.unwrap();
        store.record_play(2, &track("hated")).await.unwrap();
        store.record_play(3, &track("hidden")).await.unwrap();
        store.set_feedback(1, &track("hated"), Some(Feedback::Dislike)).await.unwrap();

        store.replace_neighbors(1, &[(2, 0.8), (3, 0.5), (1, 1.0)]).await.unwrap();
        store.update_user_prefs(3, &UserPrefsPatch { locale: None, history_enabled: Some(false) }).await.unwrap();

        let n = store.neighbors(1, 10).await.unwrap();
        assert_eq!(n, vec![Neighbor { user_id: 2, score: 0.8 }]);

        let d = store.discover_tracks(1, 10).await.unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].track.key, "popular");
        assert!((d[0].score - 2.4).abs() < 1e-9);

        store.replace_neighbors(1, &[]).await.unwrap();
        assert!(store.neighbors(1, 10).await.unwrap().is_empty());
    }
}
