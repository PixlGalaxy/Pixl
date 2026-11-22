//! One player actor per active voice channel (guild + channel), driven by commands, voice events and
//! the panel API.
//!
//! [`PlayerManager`] owns the set of players. Each [`PlayerHandle`] talks to its actor over a bounded
//! mpsc channel (every request has a timeout), exposes the latest [`PlayerSnapshot`] without asking
//! the actor, and a broadcast of [`PlayerEvent`]s (state changes and position ticks) for the panel.

pub mod actor;
pub mod lifecycle;
pub mod messages;
pub mod playback;
pub mod queue;
pub mod radio;
pub mod state;
pub mod votes;

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use dashmap::DashMap;
use serenity::all::{ChannelId, GuildId};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, OwnedMutexGuard};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    config::Config,
    discord::registry::BotRegistry,
    i18n::I18n,
    procs::{ProcOwner, ProcessRegistry},
    sources::{Playable, SourceError, Sources},
    store::{GuildSettings, LoopMode, SessionKey, SessionWriter, Store, VoteSkip},
};
use playback::TrackSignal;
pub use queue::{Entry, PushError, PushResult, QueueEntry, Requester};
pub use state::{PlayerEvent, PlayerSnapshot};

const COMMAND_QUEUE: usize = 64;
const EVENT_QUEUE: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);
/// Ended queues kept for "Play it again".
const ENDED_KEEP: Duration = Duration::from_secs(60 * 60);
const ENDED_MAX: usize = 512;

/// Everything a player needs. Shared by all players.
pub struct PlayerDeps {
    pub config: Arc<Config>,
    pub i18n: Arc<I18n>,
    pub store: Store,
    pub sources: Sources,
    pub procs: ProcessRegistry,
    pub sessions: SessionWriter,
    pub bots: Arc<BotRegistry>,
    pub ai: crate::ai::AiEngine,
    /// HTTP client for songbird stream reads.
    pub http: reqwest::Client,
}

type Reply<T> = oneshot::Sender<T>;

#[derive(Debug, Clone)]
pub enum SkipOutcome {
    Skipped { skipped: Entry, next: Option<Entry> },
    Voted { current: usize, required: usize },
    AlreadyVoted { current: usize, required: usize },
    NothingPlaying,
}

/// Why a session ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// `/stop`, Stop button or panel. `announce` posts a notice (button / panel).
    Stopped {
        by: u64,
        announce: bool,
    },
    /// Nobody came back to the empty channel.
    EmptyTimeout,
    /// The queue ended and the leave delay passed.
    QueueEnded,
    TooManyFailures,
    /// Someone disconnected the bot.
    Kicked,
    /// The voice channel was deleted.
    ChannelDeleted,
    /// The bot left the guild (or was removed).
    GuildGone,
    /// Process shutdown: the session is kept for resume.
    Shutdown,
}

pub enum Command {
    Enqueue {
        entries: Vec<Entry>,
        max: usize,
        text_channel: Option<u64>,
        reply: Reply<Result<PushResult, PushError>>,
    },
    Pause {
        reply: Reply<bool>,
    },
    Resume {
        reply: Reply<bool>,
    },
    Skip {
        user: u64,
        force: bool,
        rule: VoteSkip,
        reply: Reply<SkipOutcome>,
    },
    Previous {
        reply: Reply<Option<Entry>>,
    },
    Stop {
        by: u64,
        announce: bool,
    },
    Seek {
        position_ms: u64,
        reply: Reply<bool>,
    },
    Volume {
        volume: u16,
        reply: Reply<u16>,
    },
    Loop {
        mode: LoopMode,
        reply: Reply<()>,
    },
    Shuffle {
        reply: Reply<usize>,
    },
    Move {
        from: usize,
        to: usize,
        reply: Reply<Option<(Entry, usize)>>,
    },
    MoveUid {
        uid: Uuid,
        to_index: usize,
        reply: Reply<bool>,
    },
    Remove {
        position: usize,
        reply: Reply<Option<Entry>>,
    },
    RemoveUid {
        uid: Uuid,
        reply: Reply<Option<Entry>>,
    },
    SkipTo {
        position: usize,
        reply: Reply<Option<(usize, Entry)>>,
    },
    Clear {
        reply: Reply<usize>,
    },
    Settings(GuildSettings),
    SetTextChannel(u64),
    Humans(HashSet<u64>),
    /// The bot was moved to another voice channel (already re-keyed in the manager).
    Moved(u64),
    End(EndReason),
    // Internal
    Prepared {
        uid: Uuid,
        prefetch: bool,
        result: Box<Result<Playable, SourceError>>,
    },
    Track {
        handle_id: Uuid,
        signal: TrackSignal,
    },
    // AI radio (see `radio`)
    StartRadio {
        spec: radio::RadioSpec,
        reply: Reply<Result<radio::RadioTicket, radio::StartRefused>>,
    },
    StopRadio {
        reply: Reply<bool>,
    },
    CancelRadioJob {
        job_id: Option<Uuid>,
        reply: Reply<bool>,
    },
    /// Someone disliked a track (already stored).
    RadioDisliked {
        user: u64,
        uid: Uuid,
    },
    RadioPhase {
        id: Uuid,
        phase: crate::ai::AiPhase,
    },
    /// The early pick of a start job.
    RadioPicks {
        id: Uuid,
        picks: Vec<crate::ai::RadioPick>,
    },
    RadioDone {
        id: Uuid,
        result: Box<Result<Vec<crate::ai::RadioPick>, crate::ai::RadioError>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlayerError {
    #[error("the player is gone")]
    Gone,
    #[error("the player did not answer in time")]
    Timeout,
}

/// Handle to one player. Cheap to clone.
#[derive(Clone)]
pub struct PlayerHandle {
    pub guild_id: u64,
    pub bot_index: usize,
    /// Unique per player instance (a new player in the same channel gets a new one).
    pub generation: u64,
    voice_channel: Arc<AtomicU64>,
    tx: mpsc::Sender<Command>,
    events: broadcast::Sender<PlayerEvent>,
    snapshot: Arc<RwLock<Arc<PlayerSnapshot>>>,
}

impl PlayerHandle {
    pub fn voice_channel_id(&self) -> u64 {
        self.voice_channel.load(Ordering::Acquire)
    }

    pub fn key(&self) -> SessionKey {
        SessionKey {
            guild_id: self.guild_id,
            voice_channel_id: self.voice_channel_id(),
        }
    }

    /// Latest published state (no round trip to the actor).
    pub fn snapshot(&self) -> Arc<PlayerSnapshot> {
        match self.snapshot.read() {
            Ok(s) => s.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// State changes and position ticks. A lagging receiver misses events; it should then use
    /// `snapshot()` for a full state.
    pub fn subscribe(&self) -> broadcast::Receiver<PlayerEvent> {
        self.events.subscribe()
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    async fn send(&self, cmd: Command) -> Result<(), PlayerError> {
        match tokio::time::timeout(REQUEST_TIMEOUT, self.tx.send(cmd)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(PlayerError::Gone),
            Err(_) => Err(PlayerError::Timeout),
        }
    }

    async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T, PlayerError> {
        let (tx, rx) = oneshot::channel();
        self.send(make(tx)).await?;
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(PlayerError::Gone),
            Err(_) => Err(PlayerError::Timeout),
        }
    }

    pub async fn enqueue(
        &self,
        entries: Vec<Entry>,
        max: usize,
        text_channel: Option<u64>,
    ) -> Result<Result<PushResult, PushError>, PlayerError> {
        self.ask(|reply| Command::Enqueue {
            entries,
            max,
            text_channel,
            reply,
        })
        .await
    }

    /// Returns false when it was already paused.
    pub async fn pause(&self) -> Result<bool, PlayerError> {
        self.ask(|reply| Command::Pause { reply }).await
    }

    /// Returns false when it was not paused.
    pub async fn resume(&self) -> Result<bool, PlayerError> {
        self.ask(|reply| Command::Resume { reply }).await
    }

    /// `force`: skip directly (permission says so); otherwise the request is a vote under `rule`.
    pub async fn skip(&self, user: u64, force: bool, rule: VoteSkip) -> Result<SkipOutcome, PlayerError> {
        self.ask(|reply| Command::Skip {
            user,
            force,
            rule,
            reply,
        })
        .await
    }

    pub async fn previous(&self) -> Result<Option<Entry>, PlayerError> {
        self.ask(|reply| Command::Previous { reply }).await
    }

    /// End the session: stop, delete the queue and really disconnect.
    pub async fn stop(&self, by: u64, announce: bool) -> Result<(), PlayerError> {
        self.send(Command::Stop { by, announce }).await
    }

    pub async fn seek(&self, position_ms: u64) -> Result<bool, PlayerError> {
        self.ask(|reply| Command::Seek { position_ms, reply }).await
    }

    /// 0-200 (100 = original, passthrough). Returns the previous volume.
    pub async fn set_volume(&self, volume: u16) -> Result<u16, PlayerError> {
        self.ask(|reply| Command::Volume { volume, reply }).await
    }

    pub async fn set_loop(&self, mode: LoopMode) -> Result<(), PlayerError> {
        self.ask(|reply| Command::Loop { mode, reply }).await
    }

    pub async fn shuffle(&self) -> Result<usize, PlayerError> {
        self.ask(|reply| Command::Shuffle { reply }).await
    }

    /// 1-based positions within the upcoming list.
    pub async fn move_track(&self, from: usize, to: usize) -> Result<Option<(Entry, usize)>, PlayerError> {
        self.ask(|reply| Command::Move { from, to, reply }).await
    }

    /// Panel drag and drop: `to_index` is 0-based within `upcoming`.
    pub async fn move_uid(&self, uid: Uuid, to_index: usize) -> Result<bool, PlayerError> {
        self.ask(|reply| Command::MoveUid { uid, to_index, reply }).await
    }

    pub async fn remove(&self, position: usize) -> Result<Option<Entry>, PlayerError> {
        self.ask(|reply| Command::Remove { position, reply }).await
    }

    pub async fn remove_uid(&self, uid: Uuid) -> Result<Option<Entry>, PlayerError> {
        self.ask(|reply| Command::RemoveUid { uid, reply }).await
    }

    /// Jump to a 1-based upcoming position. Returns how many tracks were dropped and the new track.
    pub async fn skip_to(&self, position: usize) -> Result<Option<(usize, Entry)>, PlayerError> {
        self.ask(|reply| Command::SkipTo { position, reply }).await
    }

    pub async fn clear(&self) -> Result<usize, PlayerError> {
        self.ask(|reply| Command::Clear { reply }).await
    }

    /// Guild settings changed (panel): vote rules and limits.
    pub async fn settings_changed(&self, settings: GuildSettings) -> Result<(), PlayerError> {
        self.send(Command::Settings(settings)).await
    }

    pub async fn set_text_channel(&self, channel: u64) -> Result<(), PlayerError> {
        self.send(Command::SetTextChannel(channel)).await
    }

    pub(crate) async fn humans_changed(&self, humans: HashSet<u64>) -> Result<(), PlayerError> {
        self.send(Command::Humans(humans)).await
    }

    pub(crate) async fn moved(&self, to: u64) -> Result<(), PlayerError> {
        self.send(Command::Moved(to)).await
    }

    pub(crate) async fn end(&self, reason: EndReason) -> Result<(), PlayerError> {
        self.send(Command::End(reason)).await
    }
}

/// A saved queue to continue (restart resume or "Play it again").
#[derive(Debug, Clone)]
pub struct Restore {
    pub tracks: Vec<Entry>,
    pub index: usize,
    pub loop_mode: LoopMode,
    pub volume: u16,
    /// Post "Back online" when playback starts.
    pub after_restart: bool,
}

pub struct StartRequest {
    pub guild_id: u64,
    pub voice_channel_id: u64,
    pub bot_index: usize,
    pub text_channel_id: Option<u64>,
    pub started_by: u64,
    /// Humans in the voice channel right now.
    pub humans: HashSet<u64>,
    pub settings: GuildSettings,
    pub restore: Option<Restore>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    #[error("shutting down")]
    ShuttingDown,
    #[error("unknown bot")]
    NoBot,
    #[error("could not join the voice channel")]
    JoinFailed,
}

#[derive(Clone)]
pub struct PlayerManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    deps: Arc<PlayerDeps>,
    players: DashMap<SessionKey, PlayerHandle>,
    guild_locks: DashMap<u64, Arc<Mutex<()>>>,
    ended: DashMap<SessionKey, (Instant, Restore)>,
    closing: AtomicBool,
    generation: AtomicU64,
}

impl PlayerManager {
    pub fn new(deps: PlayerDeps) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                deps: Arc::new(deps),
                players: DashMap::new(),
                guild_locks: DashMap::new(),
                ended: DashMap::new(),
                closing: AtomicBool::new(false),
                generation: AtomicU64::new(1),
            }),
        }
    }

    pub fn deps(&self) -> &Arc<PlayerDeps> {
        &self.inner.deps
    }

    pub fn is_closing(&self) -> bool {
        self.inner.closing.load(Ordering::Acquire)
    }

    pub fn get(&self, guild_id: u64, voice_channel_id: u64) -> Option<PlayerHandle> {
        self.inner
            .players
            .get(&SessionKey {
                guild_id,
                voice_channel_id,
            })
            .map(|p| p.clone())
    }

    /// Players of one guild. Linear over all players; only used on guild-level events.
    pub fn in_guild(&self, guild_id: u64) -> Vec<PlayerHandle> {
        self.inner
            .players
            .iter()
            .filter(|p| p.key().guild_id == guild_id)
            .map(|p| p.clone())
            .collect()
    }

    pub fn all(&self) -> Vec<PlayerHandle> {
        self.inner.players.iter().map(|p| p.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.inner.players.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.players.is_empty()
    }

    pub fn bot_has_player(&self, guild_id: u64, bot_index: usize) -> bool {
        self.inner
            .players
            .iter()
            .any(|p| p.key().guild_id == guild_id && p.bot_index == bot_index)
    }

    /// Serializes bot assignment and player creation within one guild.
    pub async fn lock_guild(&self, guild_id: u64) -> OwnedMutexGuard<()> {
        let lock = self.inner.guild_locks.entry(guild_id).or_default().clone();
        lock.lock_owned().await
    }

    /// Join the voice channel with the given bot and start a player there.
    pub async fn start(&self, req: StartRequest) -> Result<PlayerHandle, StartError> {
        if self.is_closing() {
            return Err(StartError::ShuttingDown);
        }
        let deps = self.inner.deps.clone();
        let bot = deps.bots.bot(req.bot_index).cloned().ok_or(StartError::NoBot)?;
        let guild = GuildId::new(req.guild_id);
        let joined = tokio::time::timeout(
            JOIN_TIMEOUT,
            bot.songbird.join(guild, ChannelId::new(req.voice_channel_id)),
        )
        .await;
        let call = match joined {
            Ok(Ok(call)) => call,
            Ok(Err(e)) => {
                warn!(guild = req.guild_id, channel = req.voice_channel_id, bot = req.bot_index, error = %e, "voice join failed");
                let _ = tokio::time::timeout(Duration::from_secs(5), bot.songbird.remove(guild)).await;
                return Err(StartError::JoinFailed);
            }
            Err(_) => {
                warn!(
                    guild = req.guild_id,
                    channel = req.voice_channel_id,
                    bot = req.bot_index,
                    "voice join timed out"
                );
                let _ = tokio::time::timeout(Duration::from_secs(5), bot.songbird.remove(guild)).await;
                return Err(StartError::JoinFailed);
            }
        };
        if let Ok(mut c) = tokio::time::timeout(Duration::from_secs(5), call.lock()).await {
            let _ = tokio::time::timeout(Duration::from_secs(5), c.deafen(true)).await;
        }
        deps.bots
            .set_voice(req.guild_id, req.bot_index, Some(req.voice_channel_id));

        let generation = self.inner.generation.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
        let (events, _) = broadcast::channel(EVENT_QUEUE);
        let voice_channel = Arc::new(AtomicU64::new(req.voice_channel_id));
        let snapshot = Arc::new(RwLock::new(Arc::new(actor::initial_snapshot(&req, bot.user_id()))));
        let handle = PlayerHandle {
            guild_id: req.guild_id,
            bot_index: req.bot_index,
            generation,
            voice_channel,
            tx: tx.clone(),
            events: events.clone(),
            snapshot: snapshot.clone(),
        };
        let key = SessionKey {
            guild_id: req.guild_id,
            voice_channel_id: req.voice_channel_id,
        };
        if let Some(old) = self.inner.players.insert(key, handle.clone()) {
            warn!(?key, "replacing an existing player");
            let _ = old.end(EndReason::GuildGone).await;
        }
        info!(
            guild = req.guild_id,
            channel = req.voice_channel_id,
            bot = req.bot_index,
            "player started"
        );
        let spawn = actor::Spawn {
            manager: self.clone(),
            req,
            call,
            bot,
            generation,
            tx,
            rx,
            events,
            snapshot,
        };
        tokio::spawn(actor::run(spawn));
        Ok(handle)
    }

    /// Called by the actor when it ends.
    pub(crate) fn remove(&self, key: SessionKey, generation: u64) {
        self.inner.players.remove_if(&key, |_, h| h.generation == generation);
    }

    /// The bot was dragged to another channel: re-key the player. Returns false when another player
    /// already lives in the target channel.
    pub(crate) fn rekey(&self, handle: &PlayerHandle, to: u64) -> bool {
        let from = handle.key();
        let to_key = SessionKey {
            guild_id: from.guild_id,
            voice_channel_id: to,
        };
        if self.inner.players.contains_key(&to_key) {
            return false;
        }
        if let Some((_, h)) = self.inner.players.remove(&from) {
            h.voice_channel.store(to, Ordering::Release);
            self.inner.players.insert(to_key, h);
        }
        true
    }

    /// Keep an ended queue for "Play it again" (bounded, expires after an hour).
    pub(crate) fn remember_ended(&self, key: SessionKey, restore: Restore) {
        if restore.tracks.is_empty() {
            return;
        }
        if self.inner.ended.len() >= ENDED_MAX {
            self.inner.ended.retain(|_, (at, _)| at.elapsed() < ENDED_KEEP);
            if self.inner.ended.len() >= ENDED_MAX {
                self.inner.ended.clear();
            }
        }
        self.inner.ended.insert(key, (Instant::now(), restore));
    }

    pub fn take_ended(&self, guild_id: u64, voice_channel_id: u64) -> Option<Restore> {
        let (_, (at, r)) = self.inner.ended.remove(&SessionKey {
            guild_id,
            voice_channel_id,
        })?;
        (at.elapsed() < ENDED_KEEP).then_some(r)
    }

    /// For the process sweep: a child is orphaned when its player is gone.
    pub fn owner_alive(&self, owner: &ProcOwner) -> bool {
        self.inner.players.contains_key(&SessionKey {
            guild_id: owner.guild_id,
            voice_channel_id: owner.voice_channel_id,
        })
    }

    /// For the stream disk limit: never delete the dirs of the current or next track.
    pub fn protects_dir(&self, guild_id: u64, voice_channel_id: u64, track: &str) -> bool {
        let Some(p) = self.get(guild_id, voice_channel_id) else {
            return false;
        };
        let s = p.snapshot();
        s.current
            .iter()
            .chain(s.upcoming.iter().take(1))
            .any(|e| e.uid.to_string() == track)
    }

    /// Graceful shutdown: stop accepting new players, keep every session for resume, leave voice.
    pub async fn shutdown(&self) {
        self.inner.closing.store(true, Ordering::Release);
        let players = self.all();
        info!(players = players.len(), "stopping players");
        let mut set = tokio::task::JoinSet::new();
        for p in players {
            set.spawn(async move {
                let mut events = p.subscribe();
                if p.end(EndReason::Shutdown).await.is_ok() {
                    let _ = tokio::time::timeout(Duration::from_secs(8), async {
                        loop {
                            match events.recv().await {
                                Ok(PlayerEvent::Closed) | Err(broadcast::error::RecvError::Closed) => break,
                                _ => {}
                            }
                        }
                    })
                    .await;
                }
            });
        }
        while set.join_next().await.is_some() {}
    }
}
