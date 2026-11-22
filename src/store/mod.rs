//! SQLite persistence (single file, WAL). Owns migrations and typed repositories.

use std::{path::Path, str::FromStr, time::Duration};

use anyhow::Result;
use serde::{de::DeserializeOwned, Serialize};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    SqlitePool,
};

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new().max_connections(8).connect_with(options).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Read a cached JSON value. Expired entries count as missing.
    pub async fn cache_get<T: DeserializeOwned>(&self, namespace: &str, key: &str) -> Result<Option<T>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM cache WHERE namespace = ? AND key = ? AND expires_at > ?")
                .bind(namespace)
                .bind(key)
                .bind(now_secs())
                .fetch_optional(&self.pool)
                .await?;
        Ok(match row {
            Some((raw,)) => serde_json::from_str(&raw).ok(),
            None => None,
        })
    }

    /// Store a JSON value that expires after `ttl`.
    pub async fn cache_put<T: Serialize>(&self, namespace: &str, key: &str, value: &T, ttl: Duration) -> Result<()> {
        sqlx::query(
            "INSERT INTO cache (namespace, key, value, expires_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(namespace, key) DO UPDATE SET value = excluded.value, expires_at = excluded.expires_at",
        )
        .bind(namespace)
        .bind(key)
        .bind(serde_json::to_string(value)?)
        .bind(now_secs() + ttl.as_secs() as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete expired cache entries; returns how many were removed.
    pub async fn cache_sweep(&self) -> Result<u64> {
        let res = sqlx::query("DELETE FROM cache WHERE expires_at <= ?").bind(now_secs()).execute(&self.pool).await?;
        Ok(res.rows_affected())
    }
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
