//! SQLite persistence (single file, WAL). Owns migrations and typed repositories.
//!
//! Snowflakes are stored as INTEGER (i64); every id below 2^63 round-trips losslessly.
#![allow(dead_code)]

mod catalog;
mod channels;
mod djs;
mod feedback;
mod history;
mod maintenance;
mod meta;
mod nodes;
mod panel_sessions;
mod playlists;
mod prefs;
mod sessions;
mod settings;
mod taste;

// Re-exports for callers outside the store (unused until those subsystems land).
#[allow(unused_imports)]
pub use catalog::*;
#[allow(unused_imports)]
pub use channels::*;
#[allow(unused_imports)]
pub use djs::*;
#[allow(unused_imports)]
pub use feedback::*;
#[allow(unused_imports)]
pub use history::*;
#[allow(unused_imports)]
pub use maintenance::*;
#[allow(unused_imports)]
pub use nodes::*;
#[allow(unused_imports)]
pub use panel_sessions::*;
#[allow(unused_imports)]
pub use playlists::*;
#[allow(unused_imports)]
pub use prefs::*;
#[allow(unused_imports)]
pub use sessions::*;
#[allow(unused_imports)]
pub use settings::*;
#[allow(unused_imports)]
pub use taste::*;

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
            .foreign_keys(true)
            .pragma("temp_store", "memory")
            .pragma("cache_size", "-8000");
        let pool = SqlitePoolOptions::new().max_connections(8).connect_with(options).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    /// Private in-memory database with migrations applied (tests). One connection that never
    /// expires, because every in-memory connection is a separate database.
    pub async fn open_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Write transaction that takes the write lock up front (BEGIN IMMEDIATE), so a read followed by a
    /// write inside it waits on busy_timeout instead of failing with SQLITE_BUSY on lock upgrade.
    pub(crate) async fn begin_write(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
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

    /// Drop one cached value (forced refreshes).
    pub async fn cache_delete(&self, namespace: &str, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM cache WHERE namespace = ? AND key = ?")
            .bind(namespace)
            .bind(key)
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

/// Snowflake to SQLite INTEGER.
#[inline]
pub(crate) fn sf(id: u64) -> i64 {
    id as i64
}

/// Serde helper: snowflakes travel as strings in JSON (API contract), but numbers are accepted too.
pub mod snowflake {
    use serde::{de, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(id: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(id)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Str(String),
            Num(u64),
        }
        match Raw::deserialize(d)? {
            Raw::Str(s) => s.parse().map_err(de::Error::custom),
            Raw::Num(n) => Ok(n),
        }
    }
}

/// Serde helper for `Option<Option<T>>` patch fields: absent = None, null = Some(None).
pub(crate) fn double_option<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Option::<T>::deserialize(d).map(Some)
}

#[cfg(test)]
pub(crate) mod test_util {
    use crate::sources::{TrackInfo, TrackOrigin};

    pub fn track(id: &str) -> TrackInfo {
        TrackInfo {
            title: format!("Song {id}"),
            artists: vec![format!("Artist {id}")],
            duration_ms: Some(180_000),
            thumbnail: None,
            url: format!("https://www.youtube.com/watch?v={id}"),
            origin: TrackOrigin::Youtube,
            youtube_id: Some(id.to_owned()),
            spotify_id: None,
            isrc: None,
            music: None,
        }
    }
}
