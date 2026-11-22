//! The AI radio inside a player, and the entry points shared by Discord and the panel API.
//!
//! - **Start** (`/ai`, the "Start radio" button, `POST /api/ai/start`): [`start_radio`] checks the
//!   rules (AI enabled, user in the channel, history on, `canAdd`), assigns a bot like `/play` when
//!   the channel has no player, and asks the player to start a job. The first (algorithmic) pick is
//!   queued as soon as it exists; the rest is appended when the job is ready.
//! - **Continuous**: while the radio is active and fewer than [`REFILL_BELOW`] tracks are upcoming,
//!   the player starts one refill at a time (never past `max_queue_length`).
//! - **Stop** ([`stop_radio`], "Stop radio", `/stop`): the radio ends; queued songs keep playing.
//!   `/stop` ends the whole session and the radio with it.
//! - **Cancel** ([`cancel_radio_job`]): aborts only the running job. Cancelling the start job ends
//!   the radio (nothing more is added); cancelling a refill keeps the radio but holds refills until
//!   the next track starts, so the cancel is not undone right away.
//! - **Owner leaves**: the radio keeps going with the remaining listeners (their taste is blended
//!   on the next refill) until it is stopped or the session ends.
//! - **Dislike**: stored as feedback only (future picks avoid it); it never skips, so it cannot
//!   bypass DJ mode or vote skip.
//!
//! Permissions (api.md clarification 4): `canAdd` starts a radio; the owner or anyone with
//! `canControl` stops it or cancels a job (and may replace someone else's radio).

use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::debug;
use uuid::Uuid;

use super::{
    queue::{Entry, QueueEntry, Requester},
    state::epoch_ms,
    Command, PlayerError, PlayerHandle, PlayerManager,
};
use crate::{
    ai::{AiPhase, RadioError, RadioJob, RadioMix, RadioPick},
    discord::{
        play::{self, PlayCtx, PlayError},
        respond::actor,
    },
    perms::{can, Action, Actor, Decision, Deny},
    store::GuildSettings,
};

/// Refill when fewer upcoming tracks than this are left.
pub const REFILL_BELOW: usize = 3;
/// Picks asked for when a radio starts (the early pick included).
pub const START_BATCH: usize = 20;
pub const REFILL_BATCH: usize = 10;
/// Consecutive failed refills before the radio turns itself off.
pub const MAX_REFILL_FAILURES: u32 = 3;
/// Upper bound of one job (model load + thinking + YouTube resolution).
const JOB_TIMEOUT: Duration = Duration::from_secs(300);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const PROMPT_MAX: usize = 200;

// ---------------------------------------------------------------------------
// Snapshot / API shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiJobSnapshot {
    pub id: Uuid,
    pub phase: AiPhase,
    /// Epoch ms.
    pub started_at: i64,
    /// The job is still running (it can be cancelled by the owner or someone with `canControl`).
    pub can_cancel: bool,
}

/// The radio part of a player snapshot (`AiState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiSnapshot {
    pub active: bool,
    pub owner_id: u64,
    /// The running job, or the last one (with its final phase) until the next starts.
    pub job: Option<AiJobSnapshot>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApiAiJob {
    pub id: String,
    pub phase: AiPhase,
    pub started_at: i64,
    pub can_cancel: bool,
}

/// `AiState` of plan/api.md.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApiAiState {
    pub active: bool,
    pub owner_id: String,
    pub job: Option<ApiAiJob>,
}

impl AiSnapshot {
    pub fn to_api(&self) -> ApiAiState {
        ApiAiState {
            active: self.active,
            owner_id: self.owner_id.to_string(),
            job: self.job.as_ref().map(|j| ApiAiJob {
                id: j.id.to_string(),
                phase: j.phase,
                started_at: j.started_at,
                can_cancel: j.can_cancel,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Player-side state (owned by the actor)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Start,
    Refill,
}

/// Progress of one job, for the Discord "picking songs" card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioProgress {
    pub job_id: Uuid,
    pub phase: AiPhase,
    /// Tracks this job added to the queue (the early pick included).
    pub added: usize,
    /// The early pick is in the queue (it plays while the model works).
    pub first_queued: bool,
    /// Some picks did not fit (`max_queue_length`).
    pub truncated: bool,
    /// Why it failed (phase `failed`).
    pub error: Option<RadioError>,
}

pub(crate) struct Job {
    pub id: Uuid,
    pub kind: JobKind,
    pub phase: AiPhase,
    pub started_at: i64,
    pub cancel: CancellationToken,
    pub progress: watch::Sender<RadioProgress>,
}

impl Job {
    pub fn new(id: Uuid, kind: JobKind, cancel: CancellationToken) -> (Self, watch::Receiver<RadioProgress>) {
        let (progress, rx) = watch::channel(RadioProgress {
            job_id: id,
            phase: AiPhase::Queued,
            added: 0,
            first_queued: false,
            truncated: false,
            error: None,
        });
        (
            Self {
                id,
                kind,
                phase: AiPhase::Queued,
                started_at: epoch_ms(),
                cancel,
                progress,
            },
            rx,
        )
    }

    pub fn running(&self) -> bool {
        !self.phase.is_final()
    }

    pub fn set_phase(&mut self, phase: AiPhase) {
        self.phase = phase;
        self.progress.send_modify(|p| p.phase = phase);
    }

    pub fn added(&mut self, n: usize, truncated: bool, first: bool) {
        self.progress.send_modify(|p| {
            p.added += n;
            p.truncated |= truncated;
            p.first_queued |= first;
        });
    }

    pub fn added_total(&self) -> usize {
        self.progress.borrow().added
    }

    /// End the job now (stop / cancel / replaced): the engine is told to abort.
    pub fn abort(&mut self) {
        if self.running() {
            self.cancel.cancel();
            self.set_phase(AiPhase::Cancelled);
        }
    }

    pub fn fail(&mut self, error: RadioError) {
        self.progress.send_modify(|p| p.error = Some(error));
        self.set_phase(AiPhase::Failed);
    }
}

/// Settings of a radio, as asked for on start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioSpec {
    pub owner: Requester,
    pub prompt: Option<String>,
    pub mix: RadioMix,
    pub blend: bool,
}

pub(crate) struct Radio {
    pub spec: RadioSpec,
    pub active: bool,
    pub job: Option<Job>,
    pub failures: u32,
    /// After a cancelled refill: no refill while this track is the current one.
    pub hold: Option<Uuid>,
}

impl Radio {
    pub fn new(spec: RadioSpec, job: Job) -> Self {
        Self {
            spec,
            active: true,
            job: Some(job),
            failures: 0,
            hold: None,
        }
    }

    pub fn owner(&self) -> u64 {
        self.spec.owner.id
    }

    pub fn running(&self) -> Option<&Job> {
        self.job.as_ref().filter(|j| j.running())
    }

    pub fn running_mut(&mut self) -> Option<&mut Job> {
        self.job.as_mut().filter(|j| j.running())
    }

    /// The running job with this id.
    pub fn job_mut(&mut self, id: Uuid) -> Option<&mut Job> {
        self.running_mut().filter(|j| j.id == id)
    }

    pub fn snapshot(&self) -> AiSnapshot {
        AiSnapshot {
            active: self.active,
            owner_id: self.owner(),
            job: self.job.as_ref().map(|j| AiJobSnapshot {
                id: j.id,
                phase: j.phase,
                started_at: j.started_at,
                can_cancel: j.running(),
            }),
        }
    }

    /// How many picks a refill should ask for now, or None when no refill is due: the radio is
    /// active, no job runs, fewer than [`REFILL_BELOW`] tracks are upcoming, there is room in the
    /// queue and refills are not on hold for the current track.
    pub fn refill_size(&self, upcoming: usize, max_queue: usize, current: Option<Uuid>) -> Option<usize> {
        if !self.active || self.running().is_some() || self.failures >= MAX_REFILL_FAILURES {
            return None;
        }
        if upcoming >= REFILL_BELOW {
            return None;
        }
        if self.hold.is_some() && self.hold == current {
            return None;
        }
        let room = max_queue.saturating_sub(upcoming);
        (room > 0).then(|| REFILL_BATCH.min(room))
    }

    /// Stop the radio (the queued songs stay). Returns false when it was not active.
    pub fn stop(&mut self) -> bool {
        let was = self.active;
        self.active = false;
        if let Some(j) = self.job.as_mut() {
            j.abort();
        }
        was
    }

    /// Cancel the running job (`id`: only that one). Returns false when nothing matched.
    pub fn cancel(&mut self, id: Option<Uuid>, current: Option<Uuid>) -> bool {
        let Some(job) = self.running_mut().filter(|j| id.is_none_or(|id| j.id == id)) else {
            return false;
        };
        job.abort();
        match job.kind {
            JobKind::Start => self.active = false,
            JobKind::Refill => self.hold = current,
        }
        true
    }

    /// A job ended (`Ok` = its remaining picks were queued). Updates the job phase and the radio.
    pub fn job_done(&mut self, id: Uuid, result: Result<usize, RadioError>, current: Option<Uuid>) {
        let Some(job) = self.job_mut(id) else { return };
        let kind = job.kind;
        let total = job.added_total();
        let outcome = match result {
            Ok(_) if total == 0 => Err(RadioError::NoCandidates),
            other => other,
        };
        match outcome {
            Ok(_) => {
                job.set_phase(AiPhase::Ready);
                self.failures = 0;
            }
            Err(RadioError::Cancelled) => {
                job.set_phase(AiPhase::Cancelled);
                match kind {
                    JobKind::Start => self.active = false,
                    JobKind::Refill => self.hold = current,
                }
            }
            Err(e) => {
                job.fail(e);
                match kind {
                    JobKind::Start if total == 0 => self.active = false,
                    JobKind::Start => {}
                    JobKind::Refill => {
                        self.failures += 1;
                        if self.failures >= MAX_REFILL_FAILURES {
                            self.active = false;
                        }
                    }
                }
            }
        }
    }
}

/// Queue entries for radio picks: requested by the owner, with the reason attached.
pub(crate) fn pick_entries(picks: Vec<RadioPick>, owner: &Requester) -> Vec<Entry> {
    picks
        .into_iter()
        .map(|p| {
            let mut e = QueueEntry::new(p.track, owner.clone());
            e.reason = serde_json::to_value(&p.reason).ok();
            std::sync::Arc::new(e)
        })
        .collect()
}

/// Clean a free-text prompt (trimmed, at most 200 characters; empty = none).
pub fn clean_prompt(p: Option<&str>) -> Option<String> {
    let p = p?.trim();
    (!p.is_empty()).then(|| p.chars().take(PROMPT_MAX).collect())
}

// ---------------------------------------------------------------------------
// Job driver: forwards phases and picks of a RadioJob to the actor
// ---------------------------------------------------------------------------

async fn send(tx: &mpsc::Sender<Command>, cmd: Command) -> bool {
    matches!(tokio::time::timeout(SEND_TIMEOUT, tx.send(cmd)).await, Ok(Ok(())))
}

/// Drive one job until it ends: every phase becomes `Command::RadioPhase`, the early pick
/// `Command::RadioPicks`, the end `Command::RadioDone`. Bounded by [`JOB_TIMEOUT`]; when the player
/// is gone the job is dropped (which cancels it).
pub(crate) async fn drive(job: RadioJob, tx: mpsc::Sender<Command>) {
    let id = job.id;
    let mut phases = job.phases.clone();
    let ftx = tx.clone();
    let forward = async move {
        loop {
            let phase = *phases.borrow_and_update();
            if !send(&ftx, Command::RadioPhase { id, phase }).await || phase.is_final() {
                break;
            }
            if phases.changed().await.is_err() {
                break;
            }
        }
    };
    let ptx = tx.clone();
    let picks = async move {
        let mut job = job;
        if let Some(first) = job.first_pick().await {
            if !send(&ptx, Command::RadioPicks { id, picks: vec![first] }).await {
                return Err(RadioError::Cancelled);
            }
        }
        job.result().await
    };
    let bounded = async {
        match tokio::time::timeout(JOB_TIMEOUT, picks).await {
            Ok(r) => r,
            Err(_) => Err(RadioError::Internal("the radio job timed out".into())),
        }
    };
    let (_, result) = tokio::join!(tokio::time::timeout(JOB_TIMEOUT + SEND_TIMEOUT, forward), bounded);
    if let Err(e) = &result {
        debug!(job = %id, error = %e, "radio job ended without picks");
    }
    let _ = send(
        &tx,
        Command::RadioDone {
            id,
            result: Box::new(result),
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// Public entry points (Discord and the panel API)
// ---------------------------------------------------------------------------

/// Why no bot could start a player in the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotUnavailable {
    /// Every bot is busy; `busy_channel` is where the main bot plays (for the message).
    Busy {
        busy_channel: Option<u64>,
    },
    /// A bot is free but lacks Connect / Speak there.
    CannotJoin,
    JoinFailed,
    ShuttingDown,
}

/// Errors of the radio entry points; `code()` is the api.md error code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RadioApiError {
    #[error("not allowed")]
    NoPermission,
    #[error("history is turned off")]
    HistoryDisabled,
    #[error("not in the voice channel")]
    NotInChannel,
    #[error("no bot can play in that channel")]
    BotUnavailable(BotUnavailable),
    /// No player / no active radio / no such job.
    #[error("not found")]
    NotFound,
    /// A manager limited Lynx to other voice channels.
    #[error("Lynx may not play in that channel")]
    ChannelNotAllowed { allowed: Vec<u64> },
    #[error("the queue is full")]
    QueueFull { max: usize },
    /// `AI_ENABLED` is off (or the engine failed unexpectedly).
    #[error("the AI radio is not available")]
    Unavailable,
    /// The channel plays a jam's shared queue.
    #[error("the radio does not work in a jam")]
    InJam,
}

#[allow(dead_code)] // Used by the panel API.
impl RadioApiError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoPermission => "no_permission",
            Self::HistoryDisabled => "history_disabled",
            Self::NotInChannel => "not_in_channel",
            Self::BotUnavailable(_) => "bot_unavailable",
            Self::ChannelNotAllowed { .. } => "channel_not_allowed",
            Self::NotFound => "not_found",
            Self::QueueFull { .. } => "queue_full",
            Self::Unavailable => "unavailable",
            Self::InJam => "in_jam",
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            Self::NoPermission | Self::HistoryDisabled | Self::NotInChannel | Self::ChannelNotAllowed { .. } => 403,
            Self::NotFound => 404,
            Self::QueueFull { .. } | Self::BotUnavailable(_) | Self::InJam => 409,
            Self::Unavailable => 503,
        }
    }
}

impl From<PlayerError> for RadioApiError {
    fn from(_: PlayerError) -> Self {
        Self::NotFound
    }
}

/// What the player refuses when asked to start a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRefused {
    QueueFull { max: usize },
    Disabled,
    /// The channel is in a jam (the radio would feed every channel of it).
    InJam,
}

pub struct RadioTicket {
    pub job_id: Uuid,
    pub progress: watch::Receiver<RadioProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioOp {
    /// `replaces_other`: another user's radio is active in the channel.
    Start {
        replaces_other: bool,
    },
    StopOrCancel,
}

/// The radio permission rule (api.md clarification 4). `is_owner`: the user owns the active radio.
pub fn radio_permission(
    actor: &Actor,
    settings: &GuildSettings,
    op: RadioOp,
    is_owner: bool,
) -> Result<(), RadioApiError> {
    let control = || match can(actor, Action::Control, settings) {
        Decision::Allow => Ok(()),
        Decision::Deny(Deny::NotInChannel) => Err(RadioApiError::NotInChannel),
        _ => Err(RadioApiError::NoPermission),
    };
    match op {
        RadioOp::Start { replaces_other } => {
            match can(actor, Action::Add, settings) {
                Decision::Allow => {}
                Decision::Deny(Deny::NotInChannel) => return Err(RadioApiError::NotInChannel),
                _ => return Err(RadioApiError::NoPermission),
            }
            if replaces_other {
                control()
            } else {
                Ok(())
            }
        }
        RadioOp::StopOrCancel if is_owner => Ok(()),
        RadioOp::StopOrCancel => control(),
    }
}

fn engine_error(e: RadioError) -> RadioApiError {
    match e {
        RadioError::HistoryDisabled => RadioApiError::HistoryDisabled,
        _ => RadioApiError::Unavailable,
    }
}

/// Everything needed to start a radio.
#[derive(Debug, Clone)]
pub struct RadioStartRequest {
    pub guild_id: u64,
    /// The voice channel to play in; the user must be in it.
    pub voice_channel_id: u64,
    /// Who starts (and owns) the radio; picks are queued as requested by them.
    pub user: Requester,
    /// Member permission bits from an interaction (None from the panel: looked up).
    pub interaction_permissions: Option<u64>,
    /// Where the player posts its messages (None keeps the current one).
    pub text_channel_id: Option<u64>,
    pub prompt: Option<String>,
    pub mix: RadioMix,
    pub blend: bool,
}

pub struct RadioStarted {
    /// The player the radio runs on (the panel API answers with its state).
    #[allow(dead_code)]
    pub player: PlayerHandle,
    pub job_id: Uuid,
    /// Phases and counts of the start job (ends on ready / failed / cancelled).
    pub progress: watch::Receiver<RadioProgress>,
}

async fn settings_of(players: &PlayerManager, guild_id: u64) -> GuildSettings {
    players.deps().store.guild_settings(guild_id).await.unwrap_or_default()
}

async fn actor_of(players: &PlayerManager, guild_id: u64, user_id: u64, perms: Option<u64>, channel: u64) -> Actor {
    let deps = players.deps();
    actor(&deps.bots, &deps.store, guild_id, user_id, perms, channel, false).await
}

fn play_ctx(players: &PlayerManager) -> PlayCtx<'_> {
    let deps = players.deps();
    PlayCtx {
        players,
        bots: &deps.bots,
        store: &deps.store,
        sources: &deps.sources,
    }
}

/// The player of a channel, or a new one on the bot `/play` would pick (the shared assignment in
/// `discord::play`, guild lock held).
pub async fn get_or_start_player(
    players: &PlayerManager,
    guild_id: u64,
    voice_channel_id: u64,
    text_channel_id: Option<u64>,
    started_by: u64,
) -> Result<PlayerHandle, BotUnavailable> {
    play::player_for(
        play_ctx(players),
        guild_id,
        voice_channel_id,
        text_channel_id,
        started_by,
        None,
    )
    .await
    .map_err(|e| match e {
        PlayError::CannotJoin { .. } => BotUnavailable::CannotJoin,
        PlayError::Busy { busy_channel, .. } => BotUnavailable::Busy { busy_channel },
        PlayError::ShuttingDown => BotUnavailable::ShuttingDown,
        _ => BotUnavailable::JoinFailed,
    })
}

/// Start a radio in the user's voice channel (creating the player when needed). Replaces an
/// active radio there (someone else's only with `canControl`).
pub async fn start_radio(players: &PlayerManager, req: RadioStartRequest) -> Result<RadioStarted, RadioApiError> {
    let deps = players.deps().clone();
    if !deps.ai.enabled() {
        return Err(RadioApiError::Unavailable);
    }
    let (guild, vc, user) = (req.guild_id, req.voice_channel_id, req.user.id);
    if deps.bots.user_voice_channel(guild, user) != Some(vc) {
        return Err(RadioApiError::NotInChannel);
    }
    deps.ai.check_radio_allowed(user).await.map_err(engine_error)?;
    let settings = settings_of(players, guild).await;
    let a = actor_of(players, guild, user, req.interaction_permissions, vc).await;
    let replaces_other = players
        .get(guild, vc)
        .and_then(|p| p.snapshot().ai.clone())
        .is_some_and(|ai| ai.active && ai.owner_id != user);
    radio_permission(&a, &settings, RadioOp::Start { replaces_other }, false)?;
    play::check_voice(play_ctx(players), guild, vc, a.is_manager)
        .await
        .map_err(|e| match e {
            PlayError::VoiceNotAllowed { allowed } => RadioApiError::ChannelNotAllowed { allowed },
            _ => RadioApiError::Unavailable,
        })?;

    let player = get_or_start_player(players, guild, vc, req.text_channel_id, user)
        .await
        .map_err(RadioApiError::BotUnavailable)?;
    if let Some(c) = req.text_channel_id {
        let _ = player.set_text_channel(c).await;
    }
    let spec = RadioSpec {
        owner: req.user,
        prompt: clean_prompt(req.prompt.as_deref()),
        mix: req.mix,
        blend: req.blend,
    };
    match player.start_radio(spec).await? {
        Ok(t) => Ok(RadioStarted {
            player,
            job_id: t.job_id,
            progress: t.progress,
        }),
        Err(StartRefused::QueueFull { max }) => Err(RadioApiError::QueueFull { max }),
        Err(StartRefused::Disabled) => Err(RadioApiError::Unavailable),
        Err(StartRefused::InJam) => Err(RadioApiError::InJam),
    }
}

async fn radio_player(
    players: &PlayerManager,
    guild_id: u64,
    voice_channel_id: u64,
    user_id: u64,
    perms: Option<u64>,
) -> Result<(PlayerHandle, AiSnapshot), RadioApiError> {
    if !players.deps().ai.enabled() {
        return Err(RadioApiError::Unavailable);
    }
    let player = players.get(guild_id, voice_channel_id).ok_or(RadioApiError::NotFound)?;
    let ai = player.snapshot().ai.clone().ok_or(RadioApiError::NotFound)?;
    let settings = settings_of(players, guild_id).await;
    let a = actor_of(players, guild_id, user_id, perms, voice_channel_id).await;
    radio_permission(&a, &settings, RadioOp::StopOrCancel, ai.owner_id == user_id)?;
    Ok((player, ai))
}

/// Turn the radio off (the queued songs keep playing).
pub async fn stop_radio(
    players: &PlayerManager,
    guild_id: u64,
    voice_channel_id: u64,
    user_id: u64,
    interaction_permissions: Option<u64>,
) -> Result<(), RadioApiError> {
    let (player, _) = radio_player(players, guild_id, voice_channel_id, user_id, interaction_permissions).await?;
    if player.stop_radio().await? {
        Ok(())
    } else {
        Err(RadioApiError::NotFound)
    }
}

/// Abort the running radio job (`job_id`: only when it is still that one).
pub async fn cancel_radio_job(
    players: &PlayerManager,
    guild_id: u64,
    voice_channel_id: u64,
    user_id: u64,
    interaction_permissions: Option<u64>,
    job_id: Option<Uuid>,
) -> Result<(), RadioApiError> {
    let (player, _) = radio_player(players, guild_id, voice_channel_id, user_id, interaction_permissions).await?;
    if player.cancel_radio_job(job_id).await? {
        Ok(())
    } else {
        Err(RadioApiError::NotFound)
    }
}

impl PlayerHandle {
    pub(crate) async fn start_radio(&self, spec: RadioSpec) -> Result<Result<RadioTicket, StartRefused>, PlayerError> {
        self.ask(|reply| Command::StartRadio { spec, reply }).await
    }

    /// Returns false when no radio was active.
    pub(crate) async fn stop_radio(&self) -> Result<bool, PlayerError> {
        self.ask(|reply| Command::StopRadio { reply }).await
    }

    /// Returns false when no job (or not that one) was running.
    pub(crate) async fn cancel_radio_job(&self, job_id: Option<Uuid>) -> Result<bool, PlayerError> {
        self.ask(|reply| Command::CancelRadioJob { job_id, reply }).await
    }
}

#[cfg(test)]
mod tests;
