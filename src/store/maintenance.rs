//! Periodic housekeeping: cache and panel session sweep, PRAGMA optimize and WAL checkpoints.

use std::time::Duration;

use anyhow::Result;
use tokio::{task::JoinHandle, time::MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::Store;

pub const CACHE_SWEEP_EVERY: Duration = Duration::from_secs(60 * 60);
pub const OPTIMIZE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
pub const CHECKPOINT_EVERY: Duration = Duration::from_secs(15 * 60);

/// Result of `PRAGMA wal_checkpoint`: (busy, wal pages, checkpointed pages).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointStats {
    pub busy: bool,
    pub log_pages: i64,
    pub checkpointed_pages: i64,
}

impl Store {
    /// Remove expired cache entries, expired panel sessions and artist tags older than `cache_ttl`.
    pub async fn sweep(&self, cache_ttl: Duration) -> Result<u64> {
        Ok(self.cache_sweep().await? + self.sweep_panel_sessions().await? + self.sweep_artist_tags(cache_ttl).await?)
    }

    /// Let SQLite refresh statistics for indexes that need it.
    pub async fn optimize(&self) -> Result<()> {
        sqlx::query("PRAGMA optimize").execute(self.pool()).await?;
        Ok(())
    }

    /// Copy the WAL back into the database and truncate it.
    pub async fn wal_checkpoint(&self) -> Result<CheckpointStats> {
        let (busy, log_pages, checkpointed_pages): (i64, i64, i64) =
            sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)").fetch_one(self.pool()).await?;
        Ok(CheckpointStats { busy: busy != 0, log_pages, checkpointed_pages })
    }
}

/// Run housekeeping until `shutdown` fires: sweep hourly, optimize daily, checkpoint every 15 min.
pub fn spawn_maintenance(store: Store, cache_ttl: Duration, shutdown: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut sweep = tokio::time::interval(CACHE_SWEEP_EVERY);
        let mut optimize = tokio::time::interval(OPTIMIZE_EVERY);
        let mut checkpoint = tokio::time::interval(CHECKPOINT_EVERY);
        for t in [&mut sweep, &mut optimize, &mut checkpoint] {
            t.set_missed_tick_behavior(MissedTickBehavior::Delay);
        }
        // The first tick fires immediately; skip it for optimize/checkpoint (nothing to do at boot).
        optimize.tick().await;
        checkpoint.tick().await;

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = sweep.tick() => match store.sweep(cache_ttl).await {
                    Ok(n) if n > 0 => info!(removed = n, "cache sweep"),
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "cache sweep failed"),
                },
                _ = optimize.tick() => {
                    if let Err(e) = store.optimize().await {
                        warn!(error = %e, "PRAGMA optimize failed");
                    }
                }
                _ = checkpoint.tick() => match store.wal_checkpoint().await {
                    Ok(s) => debug!(?s, "wal checkpoint"),
                    Err(e) => warn!(error = %e, "wal checkpoint failed"),
                },
            }
        }
        // Leave a small WAL behind on shutdown.
        let _ = store.wal_checkpoint().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{test_util::track, LoopMode, SessionKey, SessionRecord, SessionWriter};

    #[tokio::test]
    async fn sweep_and_pragmas_on_file_db() {
        let dir = std::env::temp_dir().join(format!("lynx-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("lynx.db")).await.unwrap();

        store.cache_put("ns", "live", &1, Duration::from_secs(3600)).await.unwrap();
        store.cache_put("ns", "dead", &1, Duration::ZERO).await.unwrap();
        assert_eq!(store.sweep(Duration::from_secs(3600)).await.unwrap(), 1);
        assert_eq!(store.cache_get::<i32>("ns", "live").await.unwrap(), Some(1));

        // WAL mode is on and checkpoints work with concurrent writers.
        let (mode,): (String,) = sqlx::query_as("PRAGMA journal_mode").fetch_one(store.pool()).await.unwrap();
        assert_eq!(mode, "wal");
        let (writer, handle) = SessionWriter::spawn(store.clone());
        writer.save(
            SessionRecord {
                guild_id: 1,
                voice_channel_id: 2,
                bot_user_id: 3,
                text_channel_id: 4,
                current_index: 0,
                loop_mode: LoopMode::Off,
                volume: 100,
                paused: false,
                started_by: 5,
                ai_state: None,
                updated_at: 0,
            },
            Some(vec![]),
        );
        let _ = store.record_play(9, &track("a")).await.unwrap();
        writer.flush_now().await.unwrap();
        store.optimize().await.unwrap();
        let stats = store.wal_checkpoint().await.unwrap();
        assert!(!stats.busy);
        assert!(store.get_session(SessionKey { guild_id: 1, voice_channel_id: 2 }).await.unwrap().is_some());

        let token = CancellationToken::new();
        let task = spawn_maintenance(store.clone(), Duration::from_secs(3600), token.clone());
        token.cancel();
        task.await.unwrap();
        drop(writer);
        handle.await.unwrap();
        store.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
