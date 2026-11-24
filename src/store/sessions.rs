//! Saved player sessions (resume after restart) and the debounced session writer.

use std::{collections::HashMap, time::Duration};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Sqlite, SqliteConnection};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tracing::warn;

use super::{now_secs, sf, Store};
use crate::sources::{ListOrigin, StoredSource, TrackInfo, TrackSource};

/// How often the session writer flushes coalesced changes.
pub const SESSION_FLUSH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey {
    pub guild_id: u64,
    pub voice_channel_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopMode {
    #[default]
    Off,
    Track,
    Queue,
}

impl LoopMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Track => "track",
            Self::Queue => "queue",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "track" => Self::Track,
            "queue" => Self::Queue,
            _ => Self::Off,
        }
    }
}

/// Player state without its track list.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    pub guild_id: u64,
    pub voice_channel_id: u64,
    pub bot_user_id: u64,
    pub text_channel_id: u64,
    /// Index into the full track list (played + upcoming).
    pub current_index: u32,
    pub loop_mode: LoopMode,
    pub volume: u16,
    pub paused: bool,
    pub started_by: u64,
    pub ai_state: Option<serde_json::Value>,
    /// Position in the current track (time checkpoint). 0 for rows saved before it existed.
    pub position_ms: u64,
    /// Set by the store on write.
    pub updated_at: i64,
}

impl SessionRecord {
    pub fn key(&self) -> SessionKey {
        SessionKey { guild_id: self.guild_id, voice_channel_id: self.voice_channel_id }
    }
}

#[derive(Debug, Clone)]
pub struct SessionTrack {
    pub uid: String,
    pub info: TrackInfo,
    pub requested_by: u64,
    /// The list the entry was added from. Stored inside the `info` JSON (`list_source`), so rows
    /// saved before it existed still load (as `None`) and no migration is needed.
    pub source: Option<TrackSource>,
}

/// `session_tracks.info`: the `TrackInfo` fields plus the optional `list_source`.
#[derive(Serialize)]
struct StoredInfoRef<'a> {
    #[serde(flatten)]
    info: &'a TrackInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    list_source: Option<StoredSource>,
}

#[derive(Deserialize)]
struct StoredInfo {
    #[serde(flatten)]
    info: TrackInfo,
    #[serde(default)]
    list_source: Option<StoredSource>,
}

fn encode_info(t: &SessionTrack) -> serde_json::Result<String> {
    serde_json::to_string(&StoredInfoRef {
        info: &t.info,
        list_source: t.source.as_ref().map(StoredSource::from),
    })
}

#[derive(Debug, Clone)]
pub struct SavedSession {
    pub session: SessionRecord,
    /// Ordered by position.
    pub tracks: Vec<SessionTrack>,
}

type SessionRow = (u64, u64, u64, u64, u32, String, u16, bool, u64, Option<String>, i64, i64);

fn session_from_row(r: SessionRow) -> SessionRecord {
    SessionRecord {
        guild_id: r.0,
        voice_channel_id: r.1,
        bot_user_id: r.2,
        text_channel_id: r.3,
        current_index: r.4,
        loop_mode: LoopMode::parse(&r.5),
        volume: r.6,
        paused: r.7,
        started_by: r.8,
        ai_state: r.9.and_then(|s| serde_json::from_str(&s).ok()),
        updated_at: r.10,
        position_ms: r.11.max(0) as u64,
    }
}

const SESSION_COLUMNS: &str = "guild_id, voice_channel_id, bot_user_id, text_channel_id, current_index, loop_mode, \
                               volume, paused, started_by, ai_state, updated_at, position_ms";

/// Upsert the session row and, when `tracks` is given, replace its whole track list.
async fn write_session(conn: &mut SqliteConnection, s: &SessionRecord, tracks: Option<&[SessionTrack]>) -> Result<()> {
    let ai_state = s.ai_state.as_ref().map(serde_json::to_string).transpose()?;
    sqlx::query(&format!(
        "INSERT INTO sessions ({SESSION_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(guild_id, voice_channel_id) DO UPDATE SET bot_user_id = excluded.bot_user_id, \
         text_channel_id = excluded.text_channel_id, current_index = excluded.current_index, \
         loop_mode = excluded.loop_mode, volume = excluded.volume, paused = excluded.paused, \
         started_by = excluded.started_by, ai_state = excluded.ai_state, updated_at = excluded.updated_at, \
         position_ms = excluded.position_ms"
    ))
    .bind(sf(s.guild_id))
    .bind(sf(s.voice_channel_id))
    .bind(sf(s.bot_user_id))
    .bind(sf(s.text_channel_id))
    .bind(s.current_index)
    .bind(s.loop_mode.as_str())
    .bind(s.volume)
    .bind(s.paused)
    .bind(sf(s.started_by))
    .bind(ai_state)
    .bind(now_secs())
    .bind(i64::try_from(s.position_ms).unwrap_or(i64::MAX))
    .execute(&mut *conn)
    .await?;

    let Some(tracks) = tracks else { return Ok(()) };
    sqlx::query("DELETE FROM session_tracks WHERE guild_id = ? AND voice_channel_id = ?")
        .bind(sf(s.guild_id))
        .bind(sf(s.voice_channel_id))
        .execute(&mut *conn)
        .await?;
    // 6 binds per row; 500 rows stay far below SQLite's parameter limit.
    for (chunk_no, chunk) in tracks.chunks(500).enumerate() {
        let encoded = chunk.iter().map(encode_info).collect::<Result<Vec<_>, _>>()?;
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "INSERT INTO session_tracks (guild_id, voice_channel_id, position, uid, info, requested_by) ",
        );
        qb.push_values(chunk.iter().zip(encoded).enumerate(), |mut b, (i, (t, info))| {
            b.push_bind(sf(s.guild_id))
                .push_bind(sf(s.voice_channel_id))
                .push_bind((chunk_no * 500 + i) as i64)
                .push_bind(t.uid.clone())
                .push_bind(info)
                .push_bind(sf(t.requested_by));
        });
        qb.build().execute(&mut *conn).await?;
    }
    Ok(())
}

async fn delete_session_conn(conn: &mut SqliteConnection, key: SessionKey) -> Result<bool> {
    // session_tracks rows go with it (ON DELETE CASCADE).
    let res = sqlx::query("DELETE FROM sessions WHERE guild_id = ? AND voice_channel_id = ?")
        .bind(sf(key.guild_id))
        .bind(sf(key.voice_channel_id))
        .execute(&mut *conn)
        .await?;
    Ok(res.rows_affected() > 0)
}

impl Store {
    /// Write a session and its full track list in one transaction (bypasses the debounced writer).
    pub async fn save_session_full(&self, session: &SessionRecord, tracks: &[SessionTrack]) -> Result<()> {
        let mut tx = self.begin_write().await?;
        write_session(&mut tx, session, Some(tracks)).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Returns false when there was no saved session.
    pub async fn delete_session(&self, key: SessionKey) -> Result<bool> {
        let mut conn = self.pool().acquire().await?;
        delete_session_conn(&mut conn, key).await
    }

    pub async fn get_session(&self, key: SessionKey) -> Result<Option<SavedSession>> {
        let row: Option<SessionRow> = sqlx::query_as(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE guild_id = ? AND voice_channel_id = ?"
        ))
        .bind(sf(key.guild_id))
        .bind(sf(key.voice_channel_id))
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let rows: Vec<(String, String, u64)> = sqlx::query_as(
            "SELECT uid, info, requested_by FROM session_tracks WHERE guild_id = ? AND voice_channel_id = ? \
             ORDER BY position",
        )
        .bind(sf(key.guild_id))
        .bind(sf(key.voice_channel_id))
        .fetch_all(self.pool())
        .await?;
        let mut interned = HashMap::new();
        let tracks =
            rows.into_iter().filter_map(|(uid, info, by)| decode_track(uid, &info, by, &mut interned)).collect();
        Ok(Some(SavedSession { session: session_from_row(row), tracks }))
    }

    /// Every saved session with its tracks (boot-time resume). Two queries regardless of the count.
    pub async fn list_sessions(&self) -> Result<Vec<SavedSession>> {
        let rows: Vec<SessionRow> =
            sqlx::query_as(&format!("SELECT {SESSION_COLUMNS} FROM sessions ORDER BY guild_id, voice_channel_id"))
                .fetch_all(self.pool())
                .await?;
        let mut out: Vec<SavedSession> =
            rows.into_iter().map(|r| SavedSession { session: session_from_row(r), tracks: Vec::new() }).collect();
        let index: HashMap<SessionKey, usize> = out.iter().enumerate().map(|(i, s)| (s.session.key(), i)).collect();

        let tracks: Vec<(u64, u64, String, String, u64)> = sqlx::query_as(
            "SELECT guild_id, voice_channel_id, uid, info, requested_by FROM session_tracks \
             ORDER BY guild_id, voice_channel_id, position",
        )
        .fetch_all(self.pool())
        .await?;
        let mut interned = HashMap::new();
        for (guild_id, voice_channel_id, uid, info, by) in tracks {
            if let (Some(&i), Some(t)) = (
                index.get(&SessionKey { guild_id, voice_channel_id }),
                decode_track(uid, &info, by, &mut interned),
            ) {
                out[i].tracks.push(t);
            }
        }
        Ok(out)
    }
}

/// One saved row. Entries of the same list share one `ListOrigin` through `interned`.
fn decode_track(
    uid: String,
    info: &str,
    requested_by: u64,
    interned: &mut HashMap<ListOrigin, std::sync::Arc<ListOrigin>>,
) -> Option<SessionTrack> {
    match serde_json::from_str::<StoredInfo>(info) {
        Ok(StoredInfo { info, list_source }) => Some(SessionTrack {
            uid,
            info,
            requested_by,
            source: list_source.map(|s| s.into_source(interned)),
        }),
        Err(e) => {
            warn!(%uid, error = %e, "dropping undecodable saved track");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Debounced writer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum PendingOp {
    /// `tracks: None` keeps the track list already stored.
    Upsert {
        session: SessionRecord,
        tracks: Option<Vec<SessionTrack>>,
    },
    Delete,
}

/// Merge a newer op onto an older pending op for the same key.
fn coalesce(old: PendingOp, new: PendingOp) -> PendingOp {
    match (old, new) {
        (_, PendingOp::Delete) => PendingOp::Delete,
        (_, new @ PendingOp::Upsert { tracks: Some(_), .. }) => new,
        (PendingOp::Upsert { tracks, .. }, PendingOp::Upsert { session, tracks: None }) => {
            PendingOp::Upsert { session, tracks }
        }
        // Delete then a metadata-only upsert: the old tracks must not survive.
        (PendingOp::Delete, PendingOp::Upsert { session, tracks: None }) => {
            PendingOp::Upsert { session, tracks: Some(Vec::new()) }
        }
    }
}

enum Msg {
    Op(SessionKey, PendingOp),
    Flush(oneshot::Sender<Result<usize>>),
}

/// Handle to the single session-writing actor. Cheap to clone; sends never block.
///
/// Changes are coalesced per (guild, voice channel) and written in one transaction every
/// `interval`. Dropping every handle flushes what is pending and stops the actor.
#[derive(Clone)]
pub struct SessionWriter {
    tx: mpsc::UnboundedSender<Msg>,
}

impl SessionWriter {
    pub fn spawn(store: Store) -> (Self, JoinHandle<()>) {
        Self::spawn_with_interval(store, SESSION_FLUSH_INTERVAL)
    }

    pub fn spawn_with_interval(store: Store, interval: Duration) -> (Self, JoinHandle<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(run_writer(store, rx, interval));
        (Self { tx }, handle)
    }

    /// Queue a session write. Pass `tracks: None` when only the player state changed
    /// (index, pause, volume...), so the stored track list is kept as is.
    pub fn save(&self, session: SessionRecord, tracks: Option<Vec<SessionTrack>>) {
        let key = session.key();
        let _ = self.tx.send(Msg::Op(key, PendingOp::Upsert { session, tracks }));
    }

    pub fn delete(&self, key: SessionKey) {
        let _ = self.tx.send(Msg::Op(key, PendingOp::Delete));
    }

    /// Write everything queued so far and wait for it. Returns how many sessions were written.
    pub async fn flush_now(&self) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Msg::Flush(tx)).map_err(|_| anyhow!("session writer stopped"))?;
        rx.await.map_err(|_| anyhow!("session writer stopped"))?
    }
}

async fn run_writer(store: Store, mut rx: mpsc::UnboundedReceiver<Msg>, interval: Duration) {
    let mut pending: HashMap<SessionKey, PendingOp> = HashMap::new();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Msg::Op(key, op)) => push(&mut pending, key, op),
                Some(Msg::Flush(reply)) => {
                    // Drain whatever is already queued so the flush covers it.
                    let mut replies = vec![reply];
                    while let Ok(msg) = rx.try_recv() {
                        match msg {
                            Msg::Op(key, op) => push(&mut pending, key, op),
                            Msg::Flush(other) => replies.push(other),
                        }
                    }
                    let res = flush(&store, &mut pending).await.map_err(|e| e.to_string());
                    for reply in replies {
                        let _ = reply.send(res.clone().map_err(|e| anyhow!(e)));
                    }
                }
                None => {
                    if let Err(e) = flush(&store, &mut pending).await {
                        warn!(error = %e, "final session flush failed");
                    }
                    return;
                }
            },
            _ = ticker.tick() => {
                if !pending.is_empty() {
                    if let Err(e) = flush(&store, &mut pending).await {
                        warn!(error = %e, pending = pending.len(), "session flush failed, will retry");
                    }
                }
            }
        }
    }
}

fn push(pending: &mut HashMap<SessionKey, PendingOp>, key: SessionKey, op: PendingOp) {
    let merged = match pending.remove(&key) {
        Some(old) => coalesce(old, op),
        None => op,
    };
    pending.insert(key, merged);
}

/// Write all pending ops in one transaction. On failure they are put back for the next tick.
async fn flush(store: &Store, pending: &mut HashMap<SessionKey, PendingOp>) -> Result<usize> {
    if pending.is_empty() {
        return Ok(0);
    }
    let batch = std::mem::take(pending);
    match write_batch(store, &batch).await {
        Ok(()) => Ok(batch.len()),
        Err(e) => {
            *pending = batch;
            Err(e)
        }
    }
}

async fn write_batch(store: &Store, batch: &HashMap<SessionKey, PendingOp>) -> Result<()> {
    let mut tx = store.begin_write().await?;
    for (key, op) in batch {
        match op {
            PendingOp::Upsert { session, tracks } => write_session(&mut tx, session, tracks.as_deref()).await?,
            PendingOp::Delete => {
                delete_session_conn(&mut tx, *key).await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_util::track;

    fn record(guild_id: u64, vc: u64, index: u32) -> SessionRecord {
        SessionRecord {
            guild_id,
            voice_channel_id: vc,
            bot_user_id: 99,
            text_channel_id: 5,
            current_index: index,
            loop_mode: LoopMode::Queue,
            volume: 80,
            paused: false,
            started_by: 42,
            ai_state: None,
            position_ms: 0,
            updated_at: 0,
        }
    }

    fn tracks(n: usize) -> Vec<SessionTrack> {
        (0..n)
            .map(|i| SessionTrack { uid: format!("u{i}"), info: track(&format!("v{i}")), requested_by: 42, source: None })
            .collect()
    }

    #[tokio::test]
    async fn save_full_roundtrip_and_delete() {
        let store = Store::open_in_memory().await.unwrap();
        let mut s = record(1, 2, 3);
        s.ai_state = Some(serde_json::json!({"ownerId": "42"}));
        store.save_session_full(&s, &tracks(10)).await.unwrap();
        store.save_session_full(&record(1, 3, 0), &tracks(2)).await.unwrap();

        let got = store.get_session(s.key()).await.unwrap().unwrap();
        assert_eq!(got.session.current_index, 3);
        assert_eq!(got.session.loop_mode, LoopMode::Queue);
        assert_eq!(got.session.ai_state, s.ai_state);
        assert_eq!(got.tracks.len(), 10);
        assert_eq!(got.tracks[3].uid, "u3");
        assert_eq!(got.tracks[3].info.youtube_id.as_deref(), Some("v3"));

        // Replacing shrinks the list.
        store.save_session_full(&s, &tracks(4)).await.unwrap();
        let all = store.list_sessions().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].tracks.len(), 4);
        assert_eq!(all[1].tracks.len(), 2);

        assert!(store.delete_session(s.key()).await.unwrap());
        assert!(store.get_session(s.key()).await.unwrap().is_none());
        let n: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM session_tracks").fetch_one(store.pool()).await.unwrap();
        assert_eq!(n.0, 2, "cascade removed the deleted session's tracks");
    }

    #[tokio::test]
    async fn list_sources_round_trip_and_old_rows_still_load() {
        use std::sync::Arc;

        use crate::sources::SourcePlatform;
        let store = Store::open_in_memory().await.unwrap();
        let list = Arc::new(ListOrigin {
            name: "Road trip".into(),
            image: Some("https://i.scdn.co/image/x".into()),
            platform: SourcePlatform::Spotify,
            url: "https://open.spotify.com/playlist/abc".into(),
            total: 64,
        });
        let mut ts = tracks(3);
        ts[0].source = Some(TrackSource {
            list: list.clone(),
            index: 17,
        });
        ts[1].source = Some(TrackSource {
            list: list.clone(),
            index: 18,
        });
        let s = record(1, 2, 0);
        store.save_session_full(&s, &ts).await.unwrap();
        // A row written before sources existed (plain TrackInfo JSON).
        let old = serde_json::to_string(&track("old")).unwrap();
        sqlx::query(
            "INSERT INTO session_tracks (guild_id, voice_channel_id, position, uid, info, requested_by) \
             VALUES (?, ?, 3, 'u3', ?, 42)",
        )
        .bind(sf(1))
        .bind(sf(2))
        .bind(old)
        .execute(store.pool())
        .await
        .unwrap();

        for got in [
            store.get_session(s.key()).await.unwrap().unwrap(),
            store.list_sessions().await.unwrap().remove(0),
        ] {
            assert_eq!(got.tracks.len(), 4);
            let a = got.tracks[0].source.as_ref().unwrap();
            let b = got.tracks[1].source.as_ref().unwrap();
            assert_eq!((a.index, b.index, a.list.total), (17, 18, 64));
            assert_eq!(*a.list, *list);
            assert!(
                Arc::ptr_eq(&a.list, &b.list),
                "one shared origin per list after loading"
            );
            assert!(got.tracks[2].source.is_none());
            assert!(got.tracks[3].source.is_none(), "old rows load without a source");
            assert_eq!(got.tracks[3].info.youtube_id.as_deref(), Some("old"));
            assert_eq!(got.tracks[0].info.youtube_id.as_deref(), Some("v0"));
        }
        // Rows with a source still decode as a plain TrackInfo (an older binary reading them).
        let (info,): (String,) = sqlx::query_as("SELECT info FROM session_tracks WHERE position = 0")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert!(serde_json::from_str::<TrackInfo>(&info).is_ok());
        assert!(info.contains("list_source"));
    }

    #[tokio::test]
    async fn large_queue_chunks() {
        let store = Store::open_in_memory().await.unwrap();
        store.save_session_full(&record(1, 1, 0), &tracks(1203)).await.unwrap();
        let got = store.get_session(SessionKey { guild_id: 1, voice_channel_id: 1 }).await.unwrap().unwrap();
        assert_eq!(got.tracks.len(), 1203);
        assert_eq!(got.tracks[1202].uid, "u1202");
    }

    #[tokio::test]
    async fn writer_coalesces_per_key() {
        let store = Store::open_in_memory().await.unwrap();
        let (writer, handle) = SessionWriter::spawn_with_interval(store.clone(), Duration::from_secs(3600));
        writer.save(record(1, 1, 0), Some(tracks(5)));
        for i in 1..100 {
            writer.save(record(1, 1, i), None);
        }
        writer.save(record(2, 2, 0), Some(tracks(1)));
        writer.save(record(3, 3, 0), Some(tracks(1)));
        writer.delete(SessionKey { guild_id: 3, voice_channel_id: 3 });

        // Nothing written before the flush (interval is an hour).
        assert!(store.list_sessions().await.unwrap().is_empty());
        assert_eq!(writer.flush_now().await.unwrap(), 3);

        let s = store.get_session(SessionKey { guild_id: 1, voice_channel_id: 1 }).await.unwrap().unwrap();
        assert_eq!(s.session.current_index, 99);
        assert_eq!(s.tracks.len(), 5, "metadata-only upserts kept the earlier track list");
        assert!(store.get_session(SessionKey { guild_id: 3, voice_channel_id: 3 }).await.unwrap().is_none());
        assert_eq!(store.list_sessions().await.unwrap().len(), 2);

        // Delete then metadata-only upsert drops the old tracks.
        writer.delete(SessionKey { guild_id: 1, voice_channel_id: 1 });
        writer.save(record(1, 1, 0), None);
        assert_eq!(writer.flush_now().await.unwrap(), 1);
        let s = store.get_session(SessionKey { guild_id: 1, voice_channel_id: 1 }).await.unwrap().unwrap();
        assert!(s.tracks.is_empty());

        assert_eq!(writer.flush_now().await.unwrap(), 0);
        drop(writer);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn writer_flushes_on_timer_and_on_drop() {
        let store = Store::open_in_memory().await.unwrap();
        let (writer, handle) = SessionWriter::spawn_with_interval(store.clone(), Duration::from_millis(50));
        writer.save(record(1, 1, 7), Some(tracks(2)));
        tokio::time::sleep(Duration::from_millis(400)).await;
        let s = store.get_session(SessionKey { guild_id: 1, voice_channel_id: 1 }).await.unwrap().unwrap();
        assert_eq!(s.session.current_index, 7);

        let (writer2, handle2) = SessionWriter::spawn_with_interval(store.clone(), Duration::from_secs(3600));
        writer2.save(record(4, 4, 1), None);
        drop(writer2);
        handle2.await.unwrap();
        assert!(store.get_session(SessionKey { guild_id: 4, voice_channel_id: 4 }).await.unwrap().is_some());
        drop(writer);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn position_roundtrip_and_old_rows() {
        let store = Store::open_in_memory().await.unwrap();
        let mut s = record(1, 2, 3);
        s.position_ms = 123_456;
        store.save_session_full(&s, &tracks(4)).await.unwrap();
        let got = store.get_session(s.key()).await.unwrap().unwrap();
        assert_eq!(got.session.position_ms, 123_456);

        // A row written before the column existed (no position_ms given) loads with 0.
        sqlx::query(
            "INSERT INTO sessions (guild_id, voice_channel_id, bot_user_id, text_channel_id, started_by, updated_at) \
             VALUES (7, 8, 9, 0, 1, 0)",
        )
        .execute(store.pool())
        .await
        .unwrap();
        let old = store.get_session(SessionKey { guild_id: 7, voice_channel_id: 8 }).await.unwrap().unwrap();
        assert_eq!(old.session.position_ms, 0);
        let all = store.list_sessions().await.unwrap();
        assert_eq!(all.iter().map(|s| s.session.position_ms).collect::<Vec<_>>(), [123_456, 0]);
    }

    #[tokio::test]
    async fn writer_coalesces_position_checkpoints() {
        const PLAYERS: u64 = 1000;
        let store = Store::open_in_memory().await.unwrap();
        let (writer, handle) = SessionWriter::spawn_with_interval(store.clone(), Duration::from_secs(3600));
        for g in 0..PLAYERS {
            writer.save(record(g, 1, 0), Some(tracks(3)));
        }
        assert_eq!(writer.flush_now().await.unwrap(), PLAYERS as usize);

        // Many checkpoints per player between flushes: one row write per player, tracks untouched.
        for step in 1..=20u64 {
            for g in 0..PLAYERS {
                let mut r = record(g, 1, 0);
                r.position_ms = step * 5_000 + g;
                writer.save(r, None);
            }
        }
        assert_eq!(writer.flush_now().await.unwrap(), PLAYERS as usize);
        let all = store.list_sessions().await.unwrap();
        assert_eq!(all.len(), PLAYERS as usize);
        for s in &all {
            assert_eq!(s.session.position_ms, 100_000 + s.session.guild_id);
            assert_eq!(s.tracks.len(), 3, "position-only saves keep the track list");
        }
        drop(writer);
        handle.await.unwrap();
    }

    #[test]
    fn coalesce_rules() {
        let up = |i, t: Option<usize>| PendingOp::Upsert { session: record(1, 1, i), tracks: t.map(tracks) };
        assert!(matches!(coalesce(up(0, Some(1)), PendingOp::Delete), PendingOp::Delete));
        match coalesce(up(0, Some(3)), up(1, None)) {
            PendingOp::Upsert { session, tracks: Some(t) } => {
                assert_eq!(session.current_index, 1);
                assert_eq!(t.len(), 3);
            }
            other => panic!("{other:?}"),
        }
        match coalesce(PendingOp::Delete, up(1, None)) {
            PendingOp::Upsert { tracks: Some(t), .. } => assert!(t.is_empty()),
            other => panic!("{other:?}"),
        }
    }
}
