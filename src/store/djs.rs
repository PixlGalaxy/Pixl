//! Per-guild DJ list (plain user ids assigned from the panel).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{now_secs, sf, snowflake, Store};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DjEntry {
    #[serde(with = "snowflake")]
    pub user_id: u64,
    #[serde(with = "snowflake")]
    pub added_by: u64,
    pub added_at: i64,
}

impl Store {
    /// Returns false when the user already was a DJ.
    pub async fn add_dj(&self, guild_id: u64, user_id: u64, added_by: u64) -> Result<bool> {
        let res = sqlx::query(
            "INSERT INTO guild_djs (guild_id, user_id, added_by, added_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(guild_id, user_id) DO NOTHING",
        )
        .bind(sf(guild_id))
        .bind(sf(user_id))
        .bind(sf(added_by))
        .bind(now_secs())
        .execute(self.pool())
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Returns false when the user was not a DJ.
    pub async fn remove_dj(&self, guild_id: u64, user_id: u64) -> Result<bool> {
        let res = sqlx::query("DELETE FROM guild_djs WHERE guild_id = ? AND user_id = ?")
            .bind(sf(guild_id))
            .bind(sf(user_id))
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// DJs of a guild, oldest first.
    pub async fn list_djs(&self, guild_id: u64) -> Result<Vec<DjEntry>> {
        let rows: Vec<(u64, u64, i64)> = sqlx::query_as(
            "SELECT user_id, added_by, added_at FROM guild_djs WHERE guild_id = ? ORDER BY added_at, user_id",
        )
        .bind(sf(guild_id))
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(user_id, added_by, added_at)| DjEntry {
                user_id,
                added_by,
                added_at,
            })
            .collect())
    }

    pub async fn is_dj(&self, guild_id: u64, user_id: u64) -> Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM guild_djs WHERE guild_id = ? AND user_id = ?")
            .bind(sf(guild_id))
            .bind(sf(user_id))
            .fetch_optional(self.pool())
            .await?;
        Ok(row.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_remove_list() {
        let store = Store::open_in_memory().await.unwrap();
        let big = 1_234_567_890_123_456_789u64;
        assert!(store.add_dj(1, big, 9).await.unwrap());
        assert!(!store.add_dj(1, big, 9).await.unwrap());
        assert!(store.add_dj(1, 5, 9).await.unwrap());
        assert!(store.is_dj(1, big).await.unwrap());
        assert!(!store.is_dj(2, big).await.unwrap());

        let list = store.list_djs(1).await.unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|d| d.user_id == big && d.added_by == 9));

        assert!(store.remove_dj(1, big).await.unwrap());
        assert!(!store.remove_dj(1, big).await.unwrap());
        assert!(!store.is_dj(1, big).await.unwrap());

        let json = serde_json::to_value(&store.list_djs(1).await.unwrap()[0]).unwrap();
        assert_eq!(json["userId"], "5");
    }
}
