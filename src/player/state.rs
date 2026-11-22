//! Player snapshots (published on every change) and their conversion to the panel's `PlayerState`.

use std::collections::HashMap;

use serde::Serialize;

use super::{queue::Entry, radio::AiSnapshot};
use crate::{
    sources::TrackOrigin,
    store::{Feedback, LoopMode},
};

/// Snapshot versions: one counter for every player, so a newer snapshot (also of a new player in the
/// same channel) always has a higher version. The panel drops states older than the one it shows.
static VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_version() -> u64 {
    VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Playing,
    Paused,
    Loading,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PausedReason {
    User,
    EmptyChannel,
}

/// Immutable view of a player at one moment. Cheap to clone (tracks are `Arc`s).
#[derive(Debug, Clone)]
pub struct PlayerSnapshot {
    /// Increases with every published snapshot (see [`next_version`]).
    pub version: u64,
    pub guild_id: u64,
    pub channel_id: u64,
    pub text_channel_id: Option<u64>,
    /// Index into the bot registry (0 = main).
    pub bot_index: usize,
    pub bot_user_id: u64,
    pub status: Status,
    pub paused_reason: Option<PausedReason>,
    /// Epoch ms when the queue will be deleted (empty channel).
    pub empty_channel_deadline: Option<i64>,
    pub current: Option<Entry>,
    pub position_ms: u64,
    /// Epoch ms when `position_ms` was sampled.
    pub position_at: i64,
    pub upcoming: Vec<Entry>,
    /// Last 20 played, most recent first.
    pub history: Vec<Entry>,
    pub loop_mode: LoopMode,
    pub volume: u16,
    pub vote_skip_enabled: bool,
    pub skip_votes: Option<(usize, usize)>,
    /// AI radio state (None until a radio was started in this session).
    pub ai: Option<AiSnapshot>,
}

impl PlayerSnapshot {
    /// Position now, extrapolated while playing.
    pub fn position_now(&self) -> u64 {
        self.position_now_at(epoch_ms())
    }

    /// Position at `now_ms` (epoch), extrapolated while playing.
    pub fn position_now_at(&self, now_ms: i64) -> u64 {
        if self.status == Status::Playing {
            self.position_ms + (now_ms - self.position_at).max(0) as u64
        } else {
            self.position_ms
        }
    }
}

/// Events published to subscribers (panel WebSocket).
#[derive(Debug, Clone)]
pub enum PlayerEvent {
    State(std::sync::Arc<PlayerSnapshot>),
    Position {
        position_ms: u64,
        position_at: i64,
    },
    /// The session ended; the player is gone.
    Closed,
}

// ---------------------------------------------------------------------------
// API shape (plan/api.md)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApiRequester {
    pub id: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApiTrack {
    pub uid: String,
    pub title: String,
    pub artists: Vec<String>,
    pub duration_ms: Option<u64>,
    pub artwork_url: Option<String>,
    pub url: String,
    pub origin: TrackOrigin,
    pub requested_by: ApiRequester,
    pub genres: Vec<String>,
    pub reason: Option<serde_json::Value>,
    pub feedback: Option<Feedback>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApiBot {
    pub id: String,
    pub name: String,
    pub is_main: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApiSkipVotes {
    pub current: usize,
    pub required: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApiPlayerState {
    /// Snapshot version: the panel ignores a state older than the one it has.
    pub version: u64,
    pub guild_id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub bot: ApiBot,
    pub status: Status,
    pub paused_reason: Option<PausedReason>,
    pub empty_channel_deadline: Option<i64>,
    pub current: Option<ApiTrack>,
    pub position_ms: u64,
    pub position_at: i64,
    pub upcoming: Vec<ApiTrack>,
    pub history: Vec<ApiTrack>,
    #[serde(rename = "loop")]
    pub loop_mode: LoopMode,
    pub volume: u16,
    /// `AiState` (see `radio::ApiAiState`).
    pub ai: Option<serde_json::Value>,
    pub skip_votes: Option<ApiSkipVotes>,
    pub vote_skip_enabled: bool,
    pub server_now: i64,
}

/// Data the API layer looks up (names, genres, the viewer's votes) to complete a snapshot.
#[derive(Debug, Clone, Default)]
pub struct ApiContext {
    pub channel_name: String,
    pub bot_name: String,
    /// user id -> (display name, avatar URL), for requesters restored without a name.
    pub users: HashMap<u64, (String, Option<String>)>,
    /// `store::track_key` -> genres.
    pub genres: HashMap<String, Vec<String>>,
    /// `store::track_key` -> the viewer's vote.
    pub feedback: HashMap<String, Feedback>,
}

impl ApiContext {
    fn track(&self, e: &Entry) -> ApiTrack {
        let key = crate::store::track_key(&e.info);
        let (name, avatar) = match (&e.requester.name, self.users.get(&e.requester.id)) {
            (Some(n), _) => (n.clone(), e.requester.avatar_url.clone()),
            (None, Some((n, a))) => (n.clone(), a.clone()),
            (None, None) => (e.requester.id.to_string(), None),
        };
        ApiTrack {
            uid: e.uid.to_string(),
            title: e.info.title.clone(),
            artists: e.info.artists.clone(),
            duration_ms: e.info.duration_ms.filter(|d| *d > 0),
            artwork_url: e.info.thumbnail.clone(),
            url: e.info.url.clone(),
            origin: e.info.origin,
            requested_by: ApiRequester {
                id: e.requester.id.to_string(),
                name,
                avatar_url: avatar,
            },
            genres: self.genres.get(&key).cloned().unwrap_or_default(),
            reason: e.reason.clone(),
            feedback: self.feedback.get(&key).copied(),
        }
    }
}

impl PlayerSnapshot {
    /// The panel shape. The position is sampled now (`positionAt` = now), not when the snapshot was
    /// published: a snapshot can be minutes old while a track plays.
    pub fn to_api(&self, ctx: &ApiContext) -> ApiPlayerState {
        let now = epoch_ms();
        ApiPlayerState {
            version: self.version,
            guild_id: self.guild_id.to_string(),
            channel_id: self.channel_id.to_string(),
            channel_name: ctx.channel_name.clone(),
            bot: ApiBot {
                id: self.bot_user_id.to_string(),
                name: ctx.bot_name.clone(),
                is_main: self.bot_index == 0,
            },
            status: self.status,
            paused_reason: self.paused_reason,
            empty_channel_deadline: self.empty_channel_deadline,
            current: self.current.as_ref().map(|e| ctx.track(e)),
            position_ms: self.position_now_at(now),
            position_at: now,
            upcoming: self.upcoming.iter().map(|e| ctx.track(e)).collect(),
            history: self.history.iter().map(|e| ctx.track(e)).collect(),
            loop_mode: self.loop_mode,
            volume: self.volume,
            ai: self.ai_api().and_then(|a| serde_json::to_value(a).ok()),
            skip_votes: self
                .skip_votes
                .map(|(current, required)| ApiSkipVotes { current, required }),
            vote_skip_enabled: self.vote_skip_enabled,
            server_now: now,
        }
    }

    /// `AiState` for the API (`PlayerState.ai` and the WebSocket `ai` message).
    pub fn ai_api(&self) -> Option<super::radio::ApiAiState> {
        self.ai.as_ref().map(AiSnapshot::to_api)
    }

    /// Every track key in the snapshot (for batch genre / feedback lookups).
    pub fn track_keys(&self) -> Vec<String> {
        self.current
            .iter()
            .chain(self.upcoming.iter())
            .chain(self.history.iter())
            .map(|e| crate::store::track_key(&e.info))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        player::queue::{QueueEntry, Requester},
        store::test_util::track,
    };

    #[test]
    fn api_json_shape() {
        let cur = Arc::new(QueueEntry::new(
            track("a"),
            Requester {
                id: 7,
                name: Some("Ana".into()),
                avatar_url: Some("https://cdn/x.png".into()),
            },
        ));
        let next = Arc::new(QueueEntry::new(track("b"), Requester::id_only(8)));
        let snap = PlayerSnapshot {
            version: 7,
            guild_id: 1,
            channel_id: 2,
            text_channel_id: Some(3),
            bot_index: 1,
            bot_user_id: 99,
            status: Status::Paused,
            paused_reason: Some(PausedReason::EmptyChannel),
            empty_channel_deadline: Some(1_700_000_000_000),
            current: Some(cur.clone()),
            position_ms: 1500,
            position_at: 1_699_999_000_000,
            upcoming: vec![next],
            history: vec![],
            loop_mode: LoopMode::Queue,
            volume: 100,
            vote_skip_enabled: true,
            skip_votes: Some((1, 2)),
            ai: None,
        };
        let mut ctx = ApiContext {
            channel_name: "General".into(),
            bot_name: "Lynx 2".into(),
            ..Default::default()
        };
        ctx.users.insert(8, ("Ben".into(), None));
        ctx.genres
            .insert(crate::store::track_key(&cur.info), vec!["pop".into()]);
        ctx.feedback.insert(crate::store::track_key(&cur.info), Feedback::Like);
        let v = serde_json::to_value(snap.to_api(&ctx)).unwrap();
        assert_eq!(v["guildId"], "1");
        assert_eq!(
            v["bot"],
            serde_json::json!({"id": "99", "name": "Lynx 2", "isMain": false})
        );
        assert_eq!(v["status"], "paused");
        assert_eq!(v["pausedReason"], "empty_channel");
        assert_eq!(v["loop"], "queue");
        assert_eq!(v["current"]["requestedBy"]["name"], "Ana");
        assert_eq!(v["current"]["genres"][0], "pop");
        assert_eq!(v["current"]["feedback"], "like");
        assert_eq!(v["current"]["origin"], "youtube");
        assert_eq!(v["current"]["uid"], cur.uid.to_string());
        assert_eq!(v["upcoming"][0]["requestedBy"]["name"], "Ben");
        assert_eq!(v["upcoming"][0]["feedback"], serde_json::Value::Null);
        assert_eq!(v["skipVotes"], serde_json::json!({"current": 1, "required": 2}));
        assert_eq!(v["voteSkipEnabled"], true);
        assert!(v["serverNow"].as_i64().unwrap() > 1_600_000_000_000);
        assert!(v["ai"].is_null());
        assert_eq!(snap.track_keys().len(), 2);
        assert_eq!(snap.position_now(), 1500, "paused does not extrapolate");
        assert_eq!(v["version"], 7);
        assert_eq!(v["positionMs"], 1500);
    }

    #[test]
    fn api_position_is_sampled_when_sent() {
        // A snapshot published 5 minutes ago while playing (no change since): the panel must get the
        // position of now, not the one of the publish (it used to cap the age and jump back).
        let now = epoch_ms();
        let snap = PlayerSnapshot {
            version: next_version(),
            guild_id: 1,
            channel_id: 2,
            text_channel_id: None,
            bot_index: 0,
            bot_user_id: 1,
            status: Status::Playing,
            paused_reason: None,
            empty_channel_deadline: None,
            current: Some(Arc::new(QueueEntry::new(track("a"), Requester::id_only(1)))),
            position_ms: 6_000,
            position_at: now - 300_000,
            upcoming: vec![],
            history: vec![],
            loop_mode: LoopMode::Off,
            volume: 100,
            vote_skip_enabled: false,
            skip_votes: None,
            ai: None,
        };
        let api = snap.to_api(&ApiContext::default());
        assert!((306_000..306_500).contains(&api.position_ms), "{}", api.position_ms);
        assert!(api.position_at >= now && api.position_at <= api.server_now);
        assert_eq!(snap.position_now_at(now - 300_000 + 1_000), 7_000);
        assert!(next_version() > snap.version, "versions only grow");
    }
}
