//! Activity panel sessions. Rows are keyed by the SHA-256 of the token (hex), never the token itself.

use anyhow::Result;

use super::{now_secs, sf, Store};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelSessionRow {
    pub token_hash: String,
    pub user_id: u64,
    pub username: String,
    pub global_name: Option<String>,
    pub avatar_url: Option<String>,
    pub guild_id: Option<u64>,
    pub launch_channel_id: Option<u64>,
    pub created_at: i64,
    pub expires_at: i64,
}

type Row = (
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    i64,
    i64,
);

impl Store {
    pub async fn put_panel_session(&self, s: &PanelSessionRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO panel_sessions (token_hash, user_id, username, global_name, avatar_url, guild_id, \
             launch_channel_id, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(token_hash) DO UPDATE SET expires_at = excluded.expires_at",
        )
        .bind(&s.token_hash)
        .bind(sf(s.user_id))
        .bind(&s.username)
        .bind(&s.global_name)
        .bind(&s.avatar_url)
        .bind(s.guild_id.map(sf))
        .bind(s.launch_channel_id.map(sf))
        .bind(s.created_at)
        .bind(s.expires_at)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The session with this token hash, unless it expired.
    pub async fn panel_session(&self, token_hash: &str) -> Result<Option<PanelSessionRow>> {
        let row: Option<Row> = sqlx::query_as(
            "SELECT token_hash, user_id, username, global_name, avatar_url, guild_id, launch_channel_id, \
             created_at, expires_at FROM panel_sessions WHERE token_hash = ? AND expires_at > ?",
        )
        .bind(token_hash)
        .bind(now_secs())
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|r| PanelSessionRow {
            token_hash: r.0,
            user_id: r.1 as u64,
            username: r.2,
            global_name: r.3,
            avatar_url: r.4,
            guild_id: r.5.map(|v| v as u64),
            launch_channel_id: r.6.map(|v| v as u64),
            created_at: r.7,
            expires_at: r.8,
        }))
    }

    pub async fn delete_panel_session(&self, token_hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM panel_sessions WHERE token_hash = ?")
            .bind(token_hash)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Delete every session of a user (sign-out everywhere, data wipe); returns how many were removed.
    pub async fn delete_user_panel_sessions(&self, user_id: u64) -> Result<u64> {
        let res = sqlx::query("DELETE FROM panel_sessions WHERE user_id = ?")
            .bind(sf(user_id))
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }

    /// Delete expired sessions; returns how many were removed.
    pub async fn sweep_panel_sessions(&self) -> Result<u64> {
        let res = sqlx::query("DELETE FROM panel_sessions WHERE expires_at <= ?")
            .bind(now_secs())
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(hash: &str, expires_at: i64) -> PanelSessionRow {
        PanelSessionRow {
            token_hash: hash.into(),
            user_id: 10,
            username: "ana".into(),
            global_name: Some("Ana".into()),
            avatar_url: None,
            guild_id: Some(1),
            launch_channel_id: Some(200),
            created_at: now_secs(),
            expires_at,
        }
    }

    #[tokio::test]
    async fn put_get_sweep() {
        let store = Store::open_in_memory().await.unwrap();
        let live = row("live", now_secs() + 60);
        store.put_panel_session(&live).await.unwrap();
        store.put_panel_session(&row("dead", now_secs() - 1)).await.unwrap();
        assert_eq!(store.panel_session("live").await.unwrap(), Some(live));
        assert_eq!(
            store.panel_session("dead").await.unwrap(),
            None,
            "expired rows count as missing"
        );
        assert_eq!(store.sweep_panel_sessions().await.unwrap(), 1);
        store.delete_panel_session("live").await.unwrap();
        assert_eq!(store.panel_session("live").await.unwrap(), None);

        store.put_panel_session(&row("a", now_secs() + 60)).await.unwrap();
        store.put_panel_session(&row("b", now_secs() + 60)).await.unwrap();
        let other = PanelSessionRow { user_id: 11, ..row("c", now_secs() + 60) };
        store.put_panel_session(&other).await.unwrap();
        assert_eq!(store.delete_user_panel_sessions(10).await.unwrap(), 2);
        assert_eq!(store.panel_session("a").await.unwrap(), None);
        assert_eq!(store.panel_session("c").await.unwrap(), Some(other));
    }
}
