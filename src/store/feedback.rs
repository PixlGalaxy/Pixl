//! Like / dislike per user and track.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Sqlite};

use super::{
    catalog::{lookup_track_id, track_key, upsert_track, TrackId},
    history::bump_taste_version,
    now_secs, sf, Store,
};
use crate::sources::TrackInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feedback {
    Like,
    Dislike,
}

impl Feedback {
    fn value(self) -> i64 {
        match self {
            Self::Like => 1,
            Self::Dislike => -1,
        }
    }

    fn from_value(v: i64) -> Self {
        if v < 0 {
            Self::Dislike
        } else {
            Self::Like
        }
    }
}

impl Store {
    /// Set or clear (`None`) a user's vote on a track. Feedback is an explicit action, so it is
    /// stored even when history is disabled (it only filters that user's own radio).
    pub async fn set_feedback(&self, user_id: u64, info: &TrackInfo, value: Option<Feedback>) -> Result<()> {
        // Votes only feed the AI radio, which needs history: users who turned history off store nothing.
        // Callers check `history_enabled` first to tell the user why the vote was ignored.
        if value.is_some() && !self.history_enabled(user_id).await? {
            return Ok(());
        }
        let uid = sf(user_id);
        let mut tx = self.begin_write().await?;
        match value {
            Some(v) => {
                let track_id = upsert_track(&mut tx, info).await?;
                sqlx::query(
                    "INSERT INTO user_feedback (user_id, track_id, value, at) VALUES (?, ?, ?, ?) \
                     ON CONFLICT(user_id, track_id) DO UPDATE SET value = excluded.value, at = excluded.at",
                )
                .bind(uid)
                .bind(track_id)
                .bind(v.value())
                .bind(now_secs())
                .execute(&mut *tx)
                .await?;
            }
            None => {
                let Some(track_id) = lookup_track_id(&mut tx, &track_key(info)).await? else { return Ok(()) };
                sqlx::query("DELETE FROM user_feedback WHERE user_id = ? AND track_id = ?")
                    .bind(uid)
                    .bind(track_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        bump_taste_version(&mut tx, uid).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_feedback(&self, user_id: u64, info: &TrackInfo) -> Result<Option<Feedback>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT f.value FROM user_feedback f JOIN tracks t ON t.id = f.track_id WHERE f.user_id = ? AND t.key = ?",
        )
        .bind(sf(user_id))
        .bind(track_key(info))
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|r| Feedback::from_value(r.0)))
    }

    /// A user's votes for many catalog keys at once (panel queue). Keys without a vote are absent.
    pub async fn feedback_for_keys(&self, user_id: u64, keys: &[String]) -> Result<HashMap<String, Feedback>> {
        let mut out = HashMap::new();
        for chunk in keys.chunks(500) {
            let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
                "SELECT t.key, f.value FROM user_feedback f JOIN tracks t ON t.id = f.track_id WHERE f.user_id = ",
            );
            qb.push_bind(sf(user_id)).push(" AND t.key IN (");
            let mut sep = qb.separated(", ");
            for k in chunk {
                sep.push_bind(k.clone());
            }
            qb.push(")");
            let rows: Vec<(String, i64)> = qb.build_query_as().fetch_all(self.pool()).await?;
            out.extend(rows.into_iter().map(|(k, v)| (k, Feedback::from_value(v))));
        }
        Ok(out)
    }

    /// Every track disliked by any of these users (radio filter for the people listening).
    pub async fn disliked_tracks(&self, user_ids: &[u64]) -> Result<HashSet<TrackId>> {
        if user_ids.is_empty() {
            return Ok(HashSet::new());
        }
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("SELECT DISTINCT track_id FROM user_feedback WHERE value < 0 AND user_id IN (");
        let mut sep = qb.separated(", ");
        for id in user_ids {
            sep.push_bind(sf(*id));
        }
        qb.push(")");
        let rows: Vec<(i64,)> = qb.build_query_as().fetch_all(self.pool()).await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// Tracks a user liked, newest first.
    pub async fn liked_tracks(&self, user_id: u64, limit: u32) -> Result<Vec<TrackId>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT track_id FROM user_feedback WHERE user_id = ? AND value > 0 ORDER BY at DESC LIMIT ?",
        )
        .bind(sf(user_id))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{test_util::track, UserPrefsPatch};

    #[tokio::test]
    async fn ignored_when_history_off() {
        let store = Store::open_in_memory().await.unwrap();
        let patch = UserPrefsPatch { history_enabled: Some(false), ..Default::default() };
        store.update_user_prefs(7, &patch).await.unwrap();
        store.set_feedback(7, &track("a"), Some(Feedback::Like)).await.unwrap();
        assert!(store.get_feedback(7, &track("a")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_get_toggle() {
        let store = Store::open_in_memory().await.unwrap();
        assert!(store.get_feedback(1, &track("a")).await.unwrap().is_none());
        store.set_feedback(1, &track("a"), Some(Feedback::Like)).await.unwrap();
        assert_eq!(store.get_feedback(1, &track("a")).await.unwrap(), Some(Feedback::Like));
        store.set_feedback(1, &track("a"), Some(Feedback::Dislike)).await.unwrap();
        store.set_feedback(2, &track("b"), Some(Feedback::Dislike)).await.unwrap();
        store.set_feedback(3, &track("c"), Some(Feedback::Like)).await.unwrap();
        assert_eq!(store.get_feedback(1, &track("a")).await.unwrap(), Some(Feedback::Dislike));

        let disliked = store.disliked_tracks(&[1, 2, 3]).await.unwrap();
        assert_eq!(disliked.len(), 2);

        let map = store.feedback_for_keys(1, &["a".into(), "b".into()]).await.unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map["a"], Feedback::Dislike);

        store.set_feedback(1, &track("a"), None).await.unwrap();
        assert!(store.get_feedback(1, &track("a")).await.unwrap().is_none());
        // Clearing an unknown track is a no-op.
        store.set_feedback(1, &track("zzz"), None).await.unwrap();
        assert_eq!(store.liked_tracks(3, 10).await.unwrap().len(), 1);
    }
}
