//! Synchronized "jam": several voice channels (of one or more servers) play one shared queue at the
//! same position.
//!
//! A [`JamCore`] (pure, unit tested) owns the shared queue and a timeline; one small actor per jam
//! wraps it and publishes an immutable [`JamView`] on a `watch` channel. Member players follow the
//! view (they mirror the queue and line up with the timeline); they reach the jam with [`JamCmd`]s
//! (edits forwarded from their own commands, "track ready / ended / failed" reports, humans
//! changes). The jam actor never waits on a player, so there is no lock-step between them.
//!
//! Timeline: every new current track gets a new `seq`. While members prepare it the phase is
//! `Preparing`; once every member reported ready (or [`PREPARE_WAIT`] passed) it becomes
//! `Running { anchor_ms }`: the track is at position `now - anchor_ms` (epoch ms, one process so one
//! clock). Pause stores the position; resume and seek move the anchor. Members correct drift larger
//! than [`DRIFT_TOLERANCE_MS`] by seeking.
//!
//! Restarts: jams live only in memory. After a restart every channel resumes its own saved session
//! (a member's saved queue, the host's jam queue) and nobody is in a jam.

use std::{
    collections::HashSet,
    sync::Arc,
    time::Duration,
};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::Instant,
};
use tracing::{debug, info};
use uuid::Uuid;

use super::{
    queue::{Entry, PushError, PushResult, Queue},
    state::epoch_ms,
    Command, SeekOutcome, SkipOutcome,
};
use crate::store::{LoopMode, SessionKey};

/// Voice channels in one jam (host included).
pub const MAX_MEMBERS: usize = 8;
/// Jams alive at once in the process.
pub const MAX_JAMS: usize = 4096;
/// How long a new track waits for every member to be ready before it starts anyway.
pub const PREPARE_WAIT: Duration = Duration::from_secs(8);
/// Members start a ready track this long after the decision, so all of them start together.
pub const START_DELAY_MS: i64 = 400;
/// A member further than this from the jam position seeks.
pub const DRIFT_TOLERANCE_MS: u64 = 1500;
/// Drift checks of a member while its track runs (only members of a jam have this timer).
pub const DRIFT_CHECK_EVERY: Duration = Duration::from_secs(5);
/// First drift check after a start or a seek.
pub const DRIFT_CHECK_FIRST: Duration = Duration::from_secs(2);
/// Never seek this close to the end of a track (it ends on its own).
const DRIFT_END_GUARD_MS: u64 = 3000;
const COMMAND_QUEUE: usize = 128;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Prefix of the Activity `custom_id` of a jam link.
pub const CUSTOM_ID_PREFIX: &str = "jam-";

/// Who may edit the shared queue (add, remove, reorder, skip, pause, seek, loop, shuffle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditRule {
    /// People in any member channel (each still under its own server's DJ mode).
    #[default]
    Everyone,
    /// Only people in the host channel.
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JamSettings {
    /// Other voice channels of the host's server may join.
    pub allow_channels: bool,
    /// Voice channels of other servers may join.
    pub allow_servers: bool,
    pub editable_by: EditRule,
    /// No new channels may join.
    pub closed: bool,
}

impl Default for JamSettings {
    fn default() -> Self {
        Self {
            allow_channels: true,
            allow_servers: false,
            editable_by: EditRule::Everyone,
            closed: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    pub allow_channels: Option<bool>,
    pub allow_servers: Option<bool>,
    pub editable_by: Option<EditRule>,
    pub closed: Option<bool>,
}

impl JamSettings {
    pub fn apply(&mut self, p: SettingsPatch) {
        if let Some(v) = p.allow_channels {
            self.allow_channels = v;
        }
        if let Some(v) = p.allow_servers {
            self.allow_servers = v;
        }
        if let Some(v) = p.editable_by {
            self.editable_by = v;
        }
        if let Some(v) = p.closed {
            self.closed = v;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JoinRefused {
    #[error("the jam is closed to new channels")]
    Closed,
    #[error("the jam is full")]
    Full,
    #[error("this channel is already in the jam")]
    AlreadyIn,
    #[error("other channels of this server may not join")]
    ChannelsOff,
    #[error("channels of other servers may not join")]
    ServersOff,
    #[error("the jam ended")]
    Ended,
    #[error("this channel is in another jam")]
    InAnotherJam,
}

impl JoinRefused {
    /// API error code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Closed => "jam_closed",
            Self::Full => "jam_full",
            Self::AlreadyIn => "jam_already_in",
            Self::ChannelsOff => "jam_channels_off",
            Self::ServersOff => "jam_servers_off",
            Self::Ended => "jam_ended",
            Self::InAnotherJam => "jam_other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreateRefused {
    #[error("this channel is already in a jam")]
    AlreadyInJam,
    #[error("too many jams")]
    TooMany,
}

/// One voice channel of a jam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub key: SessionKey,
    /// Voice channel name (shown to the other channels).
    pub name: String,
    pub guild_name: String,
    /// Join order (the host transfer picks the lowest).
    pub joined: u64,
    pub has_humans: bool,
}

impl Member {
    pub fn new(key: SessionKey, name: String, guild_name: String, has_humans: bool) -> Self {
        Self {
            key,
            name,
            guild_name,
            joined: 0,
            has_humans,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Nothing to play (the shared queue ended or is empty).
    Stopped,
    /// Members are preparing the track; `paused` keeps a pause that came before or during it.
    Preparing { since_ms: i64, paused: bool },
    /// The track is at `now - anchor_ms`.
    Running { anchor_ms: i64 },
    Paused { position_ms: u64 },
}

impl Phase {
    pub fn paused(self) -> bool {
        matches!(
            self,
            Self::Paused { .. } | Self::Preparing { paused: true, .. }
        )
    }

    /// Position of the track at `now_ms` (negative anchors count as 0: the start is still ahead).
    pub fn position(self, now_ms: i64) -> u64 {
        match self {
            Self::Running { anchor_ms } => (now_ms - anchor_ms).max(0) as u64,
            Self::Paused { position_ms } => position_ms,
            Self::Stopped | Self::Preparing { .. } => 0,
        }
    }
}

/// What a member should do about the distance between its track and the jam position: seek to the
/// returned position, or nothing. Near the end the track is left alone (it ends on its own).
pub fn drift_correction(actual_ms: u64, expected_ms: u64, duration_ms: Option<u64>) -> Option<u64> {
    if actual_ms.abs_diff(expected_ms) <= DRIFT_TOLERANCE_MS {
        return None;
    }
    if duration_ms.is_some_and(|d| d > 0 && expected_ms + DRIFT_END_GUARD_MS >= d) {
        return None;
    }
    Some(expected_ms)
}

/// Immutable state of a jam, published on every change.
#[derive(Debug, Clone)]
pub struct JamView {
    pub id: Arc<str>,
    pub host: SessionKey,
    /// Join order; the host is one of them.
    pub members: Vec<Member>,
    pub settings: JamSettings,
    pub ended: bool,
    /// Bumped on every publish (panel change detection).
    pub rev: u64,
    /// Bumped when the track list, index or loop mode changed.
    pub queue_version: u64,
    pub tracks: Arc<Vec<Entry>>,
    pub index: usize,
    pub active: bool,
    pub loop_mode: LoopMode,
    /// Bumped on every new current track (also a restart of the same one with loop: song).
    pub seq: u64,
    pub current: Option<Uuid>,
    pub phase: Phase,
}

impl JamView {
    pub fn is_member(&self, key: SessionKey) -> bool {
        self.members.iter().any(|m| m.key == key)
    }

    pub fn member(&self, key: SessionKey) -> Option<&Member> {
        self.members.iter().find(|m| m.key == key)
    }

    pub fn host_member(&self) -> Option<&Member> {
        self.member(self.host)
    }

    pub fn may_edit(&self, key: SessionKey) -> bool {
        may_edit(&self.settings, self.host, self.is_member(key), key)
    }

    /// The same checks the jam makes when `key` asks to join.
    pub fn can_join(&self, key: SessionKey) -> Result<(), JoinRefused> {
        can_join(self.ended, &self.settings, self.host, &self.members, key)
    }

    /// The share link of the jam: opening it launches the Lynx Activity with this `custom_id`.
    pub fn share_link(&self, application_id: &str) -> String {
        share_link(application_id, &self.id)
    }
}

pub fn share_link(application_id: &str, jam_id: &str) -> String {
    format!("https://discord.com/activities/{application_id}?custom_id={CUSTOM_ID_PREFIX}{jam_id}")
}

fn may_edit(settings: &JamSettings, host: SessionKey, member: bool, key: SessionKey) -> bool {
    member && (settings.editable_by == EditRule::Everyone || key == host)
}

fn can_join(
    ended: bool,
    settings: &JamSettings,
    host: SessionKey,
    members: &[Member],
    key: SessionKey,
) -> Result<(), JoinRefused> {
    if ended {
        return Err(JoinRefused::Ended);
    }
    if members.iter().any(|m| m.key == key) {
        return Err(JoinRefused::AlreadyIn);
    }
    if settings.closed {
        return Err(JoinRefused::Closed);
    }
    if key.guild_id == host.guild_id && !settings.allow_channels {
        return Err(JoinRefused::ChannelsOff);
    }
    if key.guild_id != host.guild_id && !settings.allow_servers {
        return Err(JoinRefused::ServersOff);
    }
    if members.len() >= MAX_MEMBERS {
        return Err(JoinRefused::Full);
    }
    Ok(())
}

/// How the playing track of the host continues when a jam starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPosition {
    pub position_ms: u64,
    pub paused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LeaveOutcome {
    pub host_changed: bool,
    pub ended: bool,
}

/// The jam's state and rules, without any I/O. `now_ms` is epoch ms.
#[derive(Debug, Clone)]
pub struct JamCore {
    pub id: Arc<str>,
    pub host: SessionKey,
    pub members: Vec<Member>,
    pub settings: JamSettings,
    pub queue: Queue,
    pub queue_version: u64,
    pub seq: u64,
    pub phase: Phase,
    pub ended: bool,
    ready: HashSet<SessionKey>,
    failed: HashSet<SessionKey>,
    next_join: u64,
    rev: u64,
    tracks: (u64, Arc<Vec<Entry>>),
    /// `max_queue_length` of the host's guild (last reported): enqueues from members of other
    /// servers follow it, not their own server's limit.
    host_max_queue: Option<usize>,
}

impl JamCore {
    /// A jam seeded with the host's queue. When the host is playing its current track
    /// (`host_position`), that track goes on where it is; otherwise the jam waits (stopped) until
    /// someone adds a song or presses play.
    pub fn new(id: &str, mut host: Member, queue: Queue, host_position: Option<HostPosition>, now_ms: i64) -> Self {
        host.joined = 0;
        let key = host.key;
        let mut core = Self {
            id: id.into(),
            host: key,
            members: vec![host],
            settings: JamSettings::default(),
            queue: Queue::new(),
            queue_version: 1,
            seq: 0,
            phase: Phase::Stopped,
            ended: false,
            ready: HashSet::new(),
            failed: HashSet::new(),
            next_join: 1,
            rev: 0,
            tracks: (0, Arc::new(Vec::new())),
            host_max_queue: None,
        };
        match host_position {
            Some(p) if queue.current().is_some() => {
                core.seq = 1;
                core.phase = if p.paused {
                    Phase::Paused {
                        position_ms: p.position_ms,
                    }
                } else {
                    Phase::Running {
                        anchor_ms: now_ms - p.position_ms as i64,
                    }
                };
                core.ready.insert(key);
                core.queue = queue;
            }
            _ => {
                core.queue = queue;
                core.seq = 1;
            }
        }
        core
    }

    pub fn is_member(&self, key: SessionKey) -> bool {
        self.members.iter().any(|m| m.key == key)
    }

    pub fn may_edit(&self, key: SessionKey) -> bool {
        may_edit(&self.settings, self.host, self.is_member(key), key)
    }

    pub fn can_join(&self, key: SessionKey) -> Result<(), JoinRefused> {
        can_join(self.ended, &self.settings, self.host, &self.members, key)
    }

    pub fn position(&self, now_ms: i64) -> u64 {
        self.phase.position(now_ms)
    }

    pub fn view(&mut self) -> JamView {
        self.rev += 1;
        if self.tracks.0 != self.queue_version {
            self.tracks = (self.queue_version, Arc::new(self.queue.all().to_vec()));
        }
        JamView {
            id: self.id.clone(),
            host: self.host,
            members: self.members.clone(),
            settings: self.settings,
            ended: self.ended,
            rev: self.rev,
            queue_version: self.queue_version,
            tracks: self.tracks.1.clone(),
            index: self.queue.index(),
            active: self.queue.is_active(),
            loop_mode: self.queue.loop_mode,
            seq: self.seq,
            current: self.queue.current().map(|e| e.uid),
            phase: self.phase,
        }
    }

    fn queue_changed(&mut self) {
        self.queue_version += 1;
    }

    /// The queue's current track changed (or the queue ended): new timeline.
    fn start_track(&mut self, now_ms: i64, paused: bool) {
        self.seq += 1;
        self.ready.clear();
        self.failed.clear();
        self.phase = if self.queue.current().is_some() {
            Phase::Preparing {
                since_ms: now_ms,
                paused,
            }
        } else {
            Phase::Stopped
        };
        self.queue_changed();
    }

    fn advance(&mut self, skip: bool, now_ms: i64) -> Option<Entry> {
        let paused = self.phase.paused();
        let next = self.queue.advance(skip);
        self.start_track(now_ms, paused);
        next
    }

    /// Start the prepared track once everyone answered; skip it when nobody could play it.
    fn check_start(&mut self, now_ms: i64) -> bool {
        let Phase::Preparing { paused, .. } = self.phase else {
            return false;
        };
        let answered = self
            .members
            .iter()
            .all(|m| self.ready.contains(&m.key) || self.failed.contains(&m.key));
        if !answered {
            return false;
        }
        if self.ready.is_empty() {
            self.advance(true, now_ms);
            return true;
        }
        self.begin_running(now_ms, paused);
        true
    }

    fn begin_running(&mut self, now_ms: i64, paused: bool) {
        self.phase = if paused {
            Phase::Paused { position_ms: 0 }
        } else {
            Phase::Running {
                anchor_ms: now_ms + START_DELAY_MS,
            }
        };
    }

    /// Epoch ms when a preparing track starts anyway.
    pub fn prepare_deadline_ms(&self) -> Option<i64> {
        match self.phase {
            Phase::Preparing { since_ms, .. } => Some(since_ms + PREPARE_WAIT.as_millis() as i64),
            _ => None,
        }
    }

    /// The prepare wait is over: start with whoever is ready (late members join at the position).
    pub fn prepare_timeout(&mut self, now_ms: i64) -> bool {
        if self.prepare_deadline_ms().is_none_or(|d| now_ms < d) {
            return false;
        }
        let Phase::Preparing { paused, .. } = self.phase else {
            return false;
        };
        if !self.failed.is_empty() && self.ready.is_empty() && self.failed.len() >= self.members.len() {
            self.advance(true, now_ms);
        } else {
            self.begin_running(now_ms, paused);
        }
        true
    }

    // ---- reports from members ---------------------------------------------------------------

    pub fn ready(&mut self, key: SessionKey, seq: u64, now_ms: i64) -> bool {
        if seq != self.seq || !self.is_member(key) {
            return false;
        }
        self.ready.insert(key);
        self.check_start(now_ms)
    }

    /// A member's track ended: the shared queue moves on (once per track).
    pub fn ended_track(&mut self, key: SessionKey, seq: u64, now_ms: i64) -> bool {
        if seq != self.seq || !self.is_member(key) || self.queue.current().is_none() {
            return false;
        }
        self.advance(false, now_ms);
        true
    }

    /// A member could not play the track. It stays silent until the next one; the track is skipped
    /// only when no member can play it.
    pub fn failed_track(&mut self, key: SessionKey, seq: u64, now_ms: i64) -> bool {
        if seq != self.seq || !self.is_member(key) {
            return false;
        }
        self.ready.remove(&key);
        self.failed.insert(key);
        if self.members.iter().all(|m| self.failed.contains(&m.key)) {
            self.advance(true, now_ms);
            return true;
        }
        self.check_start(now_ms)
    }

    pub fn set_humans(&mut self, key: SessionKey, has: bool) -> bool {
        match self.members.iter_mut().find(|m| m.key == key) {
            Some(m) if m.has_humans != has => {
                m.has_humans = has;
                true
            }
            _ => false,
        }
    }

    // ---- membership ------------------------------------------------------------------------

    pub fn join(&mut self, mut member: Member, now_ms: i64) -> Result<(), JoinRefused> {
        self.can_join(member.key)?;
        member.joined = self.next_join;
        self.next_join += 1;
        self.members.push(member);
        // A new member during the wait gets its own chance to be ready (or the wait runs out).
        let _ = now_ms;
        Ok(())
    }

    /// A channel leaves (left, kicked, its player ended). The host role passes to the member that
    /// joined earliest and has people in it; without one the jam ends.
    pub fn leave(&mut self, key: SessionKey, now_ms: i64) -> Option<LeaveOutcome> {
        let i = self.members.iter().position(|m| m.key == key)?;
        self.members.remove(i);
        self.ready.remove(&key);
        self.failed.remove(&key);
        let mut out = LeaveOutcome::default();
        if self.members.is_empty() {
            self.ended = true;
            out.ended = true;
            return Some(out);
        }
        if key == self.host {
            match self.members.iter().filter(|m| m.has_humans).min_by_key(|m| m.joined) {
                Some(next) => {
                    self.host = next.key;
                    // The new host's guild limit is learned from its next report / enqueue.
                    self.host_max_queue = None;
                    out.host_changed = true;
                }
                None => {
                    self.ended = true;
                    out.ended = true;
                    return Some(out);
                }
            }
        }
        self.check_start(now_ms);
        Some(out)
    }

    /// The host removes another channel. The host itself cannot be kicked.
    pub fn kick(&mut self, key: SessionKey, now_ms: i64) -> bool {
        key != self.host && self.leave(key, now_ms).is_some()
    }

    pub fn end(&mut self) {
        self.ended = true;
    }

    // ---- shared queue and playback (the same results as a player) ---------------------------

    /// Add to the shared queue: at the end, or (`next`) right after the shared current track.
    pub fn push(&mut self, entries: Vec<Entry>, max: usize, next: bool, now_ms: i64) -> Result<PushResult, PushError> {
        let res = self.queue.push_at(entries, max, next)?;
        self.queue_changed();
        self.start_if_stopped(now_ms);
        Ok(res)
    }

    /// Start the next track of a stopped queue. True when one started.
    fn start_if_stopped(&mut self, now_ms: i64) -> bool {
        if !self.queue.is_active() && self.phase == Phase::Stopped && self.queue.start().is_some() {
            self.start_track(now_ms, false);
            true
        } else {
            false
        }
    }

    pub fn pause(&mut self, now_ms: i64) -> bool {
        match self.phase {
            Phase::Running { .. } => {
                self.phase = Phase::Paused {
                    position_ms: self.position(now_ms),
                };
                true
            }
            Phase::Preparing { since_ms, paused: false } => {
                self.phase = Phase::Preparing { since_ms, paused: true };
                true
            }
            _ => false,
        }
    }

    pub fn resume(&mut self, now_ms: i64) -> bool {
        match self.phase {
            Phase::Paused { position_ms } => {
                self.phase = Phase::Running {
                    anchor_ms: now_ms + START_DELAY_MS - position_ms as i64,
                };
                true
            }
            Phase::Preparing { since_ms, paused: true } => {
                self.phase = Phase::Preparing { since_ms, paused: false };
                true
            }
            Phase::Stopped => self.start_if_stopped(now_ms),
            _ => false,
        }
    }

    pub fn seek(&mut self, position_ms: u64, now_ms: i64) -> SeekOutcome {
        let Some(cur) = self.queue.current() else {
            return SeekOutcome::NothingPlaying;
        };
        let duration = cur.info.duration_ms.filter(|d| *d > 0);
        if duration.is_some_and(|d| position_ms > d) {
            return SeekOutcome::OutOfRange;
        }
        let target = duration
            .map(|d| position_ms.min(d.saturating_sub(1000)))
            .unwrap_or(position_ms);
        self.phase = if self.phase.paused() {
            Phase::Paused { position_ms: target }
        } else {
            Phase::Running {
                anchor_ms: now_ms - target as i64,
            }
        };
        SeekOutcome::Done
    }

    pub fn skip(&mut self, now_ms: i64) -> SkipOutcome {
        let Some(skipped) = self.queue.current().cloned() else {
            return SkipOutcome::NothingPlaying;
        };
        let next = self.advance(true, now_ms);
        SkipOutcome::Skipped { skipped, next }
    }

    pub fn previous(&mut self, now_ms: i64) -> Option<Entry> {
        if self.queue.index() == 0 {
            return None;
        }
        let paused = self.phase.paused();
        let prev = self.queue.previous()?;
        self.start_track(now_ms, paused);
        Some(prev)
    }

    pub fn set_loop(&mut self, mode: LoopMode) {
        self.queue.loop_mode = mode;
        self.queue_changed();
    }

    pub fn skip_to(&mut self, position: usize, now_ms: i64) -> Option<(usize, Entry)> {
        if position == 0 || position > self.queue.upcoming().len() {
            return None;
        }
        let paused = self.phase.paused();
        let (dropped, next) = self.queue.skip_to(position)?;
        self.start_track(now_ms, paused);
        Some((dropped.len(), next))
    }

    /// Apply a queue edit forwarded by a member player. Volume, stop and the other per-channel
    /// commands never reach the jam.
    fn edit(&mut self, cmd: Command, rng: &mut fastrand::Rng, now_ms: i64) -> bool {
        match cmd {
            Command::Enqueue {
                entries,
                max,
                next,
                reply,
                ..
            } => {
                let _ = reply.send(self.push(entries, max, next, now_ms));
            }
            Command::Pause { reply } => {
                let _ = reply.send(self.pause(now_ms));
            }
            Command::Resume { reply } => {
                let _ = reply.send(self.resume(now_ms));
            }
            Command::Skip { reply, expected, .. } => {
                let outcome = match (expected, self.queue.current()) {
                    // Another member skipped first (or a double click): don't skip the next one.
                    (Some(uid), Some(e)) if e.uid != uid => SkipOutcome::Outdated,
                    _ => self.skip(now_ms),
                };
                let _ = reply.send(outcome);
            }
            Command::Previous { reply } => {
                let _ = reply.send(self.previous(now_ms));
            }
            Command::Seek { position_ms, reply, .. } => {
                let _ = reply.send(self.seek(position_ms, now_ms));
            }
            Command::Loop { mode, reply } => {
                self.set_loop(mode);
                let _ = reply.send(());
            }
            Command::Shuffle { reply } => {
                let n = self.queue.shuffle(rng);
                self.queue_changed();
                let _ = reply.send(n);
            }
            Command::Move { from, to, reply } => {
                let r = self.queue.move_track(from, to);
                if r.is_some() {
                    self.queue_changed();
                }
                let _ = reply.send(r);
            }
            Command::MoveUid { uid, to_index, reply } => {
                let r = self
                    .queue
                    .position_of(uid)
                    .and_then(|from| self.queue.move_track(from, to_index + 1));
                if r.is_some() {
                    self.queue_changed();
                }
                let _ = reply.send(r.is_some());
            }
            Command::Remove { position, reply } => {
                let r = self.queue.remove(position);
                if r.is_some() {
                    self.queue_changed();
                }
                let _ = reply.send(r);
            }
            Command::RemoveUid { uid, reply } => {
                let r = self.queue.remove_uid(uid);
                if r.is_some() {
                    self.queue_changed();
                }
                let _ = reply.send(r);
            }
            Command::SkipTo { position, reply } => {
                let _ = reply.send(self.skip_to(position, now_ms));
            }
            Command::Clear { reply } => {
                let removed = self.queue.clear_upcoming();
                if !removed.is_empty() {
                    self.queue_changed();
                }
                let _ = reply.send(removed.len());
            }
            _ => return false,
        }
        true
    }

    /// The limit an enqueue from `from` follows: the host's server's (its own enqueues report it),
    /// also for members of other servers. Unknown yet: the sender's own.
    fn queue_limit(&mut self, from: SessionKey, sender_max: usize) -> usize {
        if from.guild_id == self.host.guild_id {
            self.host_max_queue = Some(sender_max);
            sender_max
        } else {
            self.host_max_queue.unwrap_or(sender_max)
        }
    }

    /// An edit from a member that may edit: enqueues follow the host server's queue limit.
    fn edit_from(&mut self, from: SessionKey, cmd: Command, rng: &mut fastrand::Rng, now_ms: i64) -> bool {
        let cmd = match cmd {
            Command::Enqueue {
                entries,
                max,
                next,
                text_channel,
                reply,
            } => Command::Enqueue {
                entries,
                max: self.queue_limit(from, max),
                next,
                text_channel,
                reply,
            },
            cmd => cmd,
        };
        self.edit(cmd, rng, now_ms)
    }

    /// An edit from a channel that may not edit: answered with "nothing changed" (an enqueue with
    /// "not allowed", so the user learns why).
    fn refuse(cmd: Command) {
        match cmd {
            Command::Enqueue { reply, .. } => {
                let _ = reply.send(Err(PushError::NotAllowed));
            }
            Command::Pause { reply } | Command::Resume { reply } => {
                let _ = reply.send(false);
            }
            Command::Skip { reply, .. } => {
                let _ = reply.send(SkipOutcome::NothingPlaying);
            }
            Command::Previous { reply } => {
                let _ = reply.send(None);
            }
            Command::Seek { reply, .. } => {
                let _ = reply.send(SeekOutcome::NothingPlaying);
            }
            Command::Loop { reply, .. } => {
                let _ = reply.send(());
            }
            Command::Shuffle { reply } | Command::Clear { reply } => {
                let _ = reply.send(0);
            }
            Command::Move { reply, .. } => {
                let _ = reply.send(None);
            }
            Command::MoveUid { reply, .. } => {
                let _ = reply.send(false);
            }
            Command::Remove { reply, .. } | Command::RemoveUid { reply, .. } => {
                let _ = reply.send(None);
            }
            Command::SkipTo { reply, .. } => {
                let _ = reply.send(None);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Actor
// ---------------------------------------------------------------------------

pub enum JamCmd {
    Join {
        member: Member,
        reply: oneshot::Sender<Result<(), JoinRefused>>,
    },
    Leave {
        key: SessionKey,
    },
    Humans {
        key: SessionKey,
        has: bool,
    },
    /// A shared-queue command of a member (its reply goes straight to the requester).
    Edit {
        from: SessionKey,
        cmd: Command,
    },
    Ready {
        key: SessionKey,
        seq: u64,
    },
    Ended {
        key: SessionKey,
        seq: u64,
    },
    Failed {
        key: SessionKey,
        seq: u64,
    },
    /// A member's own vote to skip passed.
    Skip {
        key: SessionKey,
        seq: u64,
    },
    Settings {
        patch: SettingsPatch,
        reply: oneshot::Sender<JamSettings>,
    },
    Kick {
        key: SessionKey,
        reply: oneshot::Sender<bool>,
    },
    /// The host server's `max_queue_length` (on create and when its settings change).
    HostLimit {
        key: SessionKey,
        max: usize,
    },
    End,
}

/// Handle to one jam. Cheap to clone.
#[derive(Clone)]
pub struct JamHandle {
    pub id: Arc<str>,
    tx: mpsc::Sender<JamCmd>,
    view: watch::Receiver<Arc<JamView>>,
}

impl JamHandle {
    pub fn view(&self) -> Arc<JamView> {
        self.view.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<JamView>> {
        self.view.clone()
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Fire and forget. A full queue retries in the background so leave / report messages are not lost.
    pub fn post(&self, cmd: JamCmd) {
        match self.tx.try_send(cmd) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(cmd)) => {
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(REQUEST_TIMEOUT, tx.send(cmd)).await;
                });
            }
        }
    }

    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> JamCmd) -> Option<T> {
        let (tx, rx) = oneshot::channel();
        tokio::time::timeout(REQUEST_TIMEOUT, self.tx.send(make(tx)))
            .await
            .ok()?
            .ok()?;
        tokio::time::timeout(REQUEST_TIMEOUT, rx).await.ok()?.ok()
    }

    pub async fn join(&self, member: Member) -> Result<(), JoinRefused> {
        self.ask(|reply| JamCmd::Join { member, reply })
            .await
            .unwrap_or(Err(JoinRefused::Ended))
    }

    pub async fn settings(&self, patch: SettingsPatch) -> Option<JamSettings> {
        self.ask(|reply| JamCmd::Settings { patch, reply }).await
    }

    pub async fn kick(&self, key: SessionKey) -> bool {
        self.ask(|reply| JamCmd::Kick { key, reply }).await.unwrap_or(false)
    }

    pub fn end(&self) {
        self.post(JamCmd::End);
    }
}

/// Every live jam, by id and by member channel. Bounded by [`MAX_JAMS`] and [`MAX_MEMBERS`].
#[derive(Clone, Default)]
pub struct Jams {
    inner: Arc<JamsInner>,
}

#[derive(Default)]
struct JamsInner {
    by_id: DashMap<Arc<str>, JamHandle>,
    by_member: DashMap<SessionKey, Arc<str>>,
}

impl Jams {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &str) -> Option<JamHandle> {
        self.inner.by_id.get(id).map(|h| h.clone()).filter(|h| !h.is_closed())
    }

    /// The jam a channel is in.
    pub fn of(&self, key: SessionKey) -> Option<JamHandle> {
        let id = self.inner.by_member.get(&key)?.clone();
        self.get(&id)
    }

    pub fn len(&self) -> usize {
        self.inner.by_id.len()
    }

    /// The shared-queue rule for `key`: true outside a jam.
    pub fn edit_allowed(&self, key: SessionKey) -> bool {
        self.of(key).is_none_or(|j| j.view().may_edit(key))
    }

    /// Start a jam hosted by `host` (called by the host's player actor with its own queue).
    pub fn create(
        &self,
        host: Member,
        queue: Queue,
        host_position: Option<HostPosition>,
    ) -> Result<JamHandle, CreateRefused> {
        if self.inner.by_member.contains_key(&host.key) {
            return Err(CreateRefused::AlreadyInJam);
        }
        if self.inner.by_id.len() >= MAX_JAMS {
            return Err(CreateRefused::TooMany);
        }
        let id: Arc<str> = new_id().into();
        let key = host.key;
        let mut core = JamCore::new(&id, host, queue, host_position, epoch_ms());
        let (view_tx, view) = watch::channel(Arc::new(core.view()));
        let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
        let handle = JamHandle {
            id: id.clone(),
            tx,
            view,
        };
        self.inner.by_id.insert(id.clone(), handle.clone());
        self.inner.by_member.insert(key, id.clone());
        info!(jam = %id, guild = key.guild_id, channel = key.voice_channel_id, "jam started");
        tokio::spawn(run(core, rx, view_tx, self.clone()));
        Ok(handle)
    }

    fn sync_members(&self, id: &Arc<str>, before: &[SessionKey], after: &[Member]) {
        for k in before {
            if !after.iter().any(|m| m.key == *k) {
                self.inner.by_member.remove_if(k, |_, v| v == id);
            }
        }
        for m in after {
            self.inner.by_member.insert(m.key, id.clone());
        }
    }

    fn remove(&self, id: &Arc<str>, members: &[SessionKey]) {
        for k in members {
            self.inner.by_member.remove_if(k, |_, v| v == id);
        }
        self.inner.by_id.remove(id);
    }
}

/// Length of a new jam id: 14 characters of a 32-letter alphabet (70 bits from the OS random source),
/// so ids can't be guessed or enumerated.
pub const ID_LEN: usize = 14;
const ID_ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";

fn new_id() -> String {
    let mut buf = [0u8; ID_LEN];
    getrandom::fill(&mut buf).expect("OS random source");
    id_from_bytes(&buf)
}

/// 256 is a multiple of 32, so the low 5 bits of a uniform byte are uniform.
fn id_from_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| ID_ALPHABET[(*b & 31) as usize] as char).collect()
}

/// Jam ids in links and API bodies: [`new_id`] (jams made before used 10 characters), optionally with
/// the `custom_id` prefix.
pub fn parse_id(raw: &str) -> Option<&str> {
    let id = raw.trim();
    let id = id.strip_prefix(CUSTOM_ID_PREFIX).unwrap_or(id);
    ((10..=16).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())).then_some(id)
}

async fn until_ms(deadline_ms: Option<i64>) {
    match deadline_ms {
        Some(d) => {
            let wait = (d - epoch_ms()).max(0) as u64;
            tokio::time::sleep_until(Instant::now() + Duration::from_millis(wait)).await
        }
        None => std::future::pending().await,
    }
}

async fn run(mut core: JamCore, mut rx: mpsc::Receiver<JamCmd>, view_tx: watch::Sender<Arc<JamView>>, jams: Jams) {
    let mut rng = fastrand::Rng::new();
    let id = core.id.clone();
    loop {
        let before: Vec<SessionKey> = core.members.iter().map(|m| m.key).collect();
        let changed = tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(cmd) => handle(&mut core, cmd, &mut rng),
                None => { core.end(); true }
            },
            _ = until_ms(core.prepare_deadline_ms()) => core.prepare_timeout(epoch_ms()),
        };
        if !changed {
            continue;
        }
        jams.sync_members(&id, &before, &core.members);
        let view = Arc::new(core.view());
        view_tx.send_replace(view);
        if core.ended {
            let all: Vec<SessionKey> = before.into_iter().chain(core.members.iter().map(|m| m.key)).collect();
            jams.remove(&id, &all);
            info!(jam = %id, "jam ended");
            // Late commands get dropped replies.
            rx.close();
            while rx.try_recv().is_ok() {}
            return;
        }
    }
}

/// Apply one command; true when the view changed (the caller publishes it).
fn handle(core: &mut JamCore, cmd: JamCmd, rng: &mut fastrand::Rng) -> bool {
    let now = epoch_ms();
    match cmd {
        JamCmd::Join { member, reply } => {
            let key = member.key;
            let r = core.join(member, now);
            let ok = r.is_ok();
            if ok {
                info!(jam = %core.id, guild = key.guild_id, channel = key.voice_channel_id, "channel joined the jam");
            }
            // The view with the new member is published right after; the joiner follows it.
            let _ = reply.send(r);
            ok
        }
        JamCmd::Leave { key } => match core.leave(key, now) {
            Some(out) => {
                debug!(jam = %core.id, channel = key.voice_channel_id, ?out, "channel left the jam");
                true
            }
            None => false,
        },
        JamCmd::Humans { key, has } => core.set_humans(key, has),
        JamCmd::Edit { from, cmd } => {
            if core.may_edit(from) {
                core.edit_from(from, cmd, rng, now)
            } else {
                JamCore::refuse(cmd);
                false
            }
        }
        JamCmd::Ready { key, seq } => core.ready(key, seq, now),
        JamCmd::Ended { key, seq } => core.ended_track(key, seq, now),
        JamCmd::Failed { key, seq } => core.failed_track(key, seq, now),
        JamCmd::Skip { key, seq } => {
            if seq == core.seq && core.may_edit(key) {
                core.skip(now);
                true
            } else {
                false
            }
        }
        JamCmd::Settings { patch, reply } => {
            core.settings.apply(patch);
            let _ = reply.send(core.settings);
            true
        }
        JamCmd::Kick { key, reply } => {
            let done = core.kick(key, now);
            let _ = reply.send(done);
            done
        }
        JamCmd::HostLimit { key, max } => {
            // Only from the current host channel's player (the limit of its server).
            if key == core.host {
                core.host_max_queue = Some(max);
            }
            false
        }
        JamCmd::End => {
            core.end();
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        player::queue::{QueueEntry, Requester},
        store::test_util::track,
    };

    const G1: u64 = 1;
    const G2: u64 = 2;

    fn key(g: u64, c: u64) -> SessionKey {
        SessionKey {
            guild_id: g,
            voice_channel_id: c,
        }
    }

    fn member(g: u64, c: u64) -> Member {
        Member::new(key(g, c), format!("vc{c}"), format!("g{g}"), true)
    }

    fn entries(ids: &[&str]) -> Vec<Entry> {
        ids.iter()
            .map(|id| Arc::new(QueueEntry::new(track(id), Requester::id_only(1))))
            .collect()
    }

    /// Host queue a, b, c with `a` playing at 10 s.
    fn playing_jam(now: i64) -> JamCore {
        let mut q = Queue::new();
        q.push(entries(&["a", "b", "c"]), 100).unwrap();
        q.start();
        JamCore::new(
            "abcdefghij",
            member(G1, 10),
            q,
            Some(HostPosition {
                position_ms: 10_000,
                paused: false,
            }),
            now,
        )
    }

    #[test]
    fn the_host_track_goes_on_where_it_is() {
        let now = 1_000_000;
        let core = playing_jam(now);
        assert_eq!(core.phase, Phase::Running { anchor_ms: now - 10_000 });
        assert_eq!(core.position(now + 5_000), 15_000);
        assert_eq!(core.seq, 1);
        assert_eq!(core.queue.current().unwrap().info.youtube_id.as_deref(), Some("a"));

        // Without a playing track the jam waits; play starts the next one.
        let mut q = Queue::new();
        q.push(entries(&["a"]), 100).unwrap();
        let mut core = JamCore::new("abcdefghij", member(G1, 10), q, None, now);
        assert_eq!(core.phase, Phase::Stopped);
        assert!(core.resume(now));
        assert!(matches!(core.phase, Phase::Preparing { paused: false, .. }));
        // Empty queue: stopped until something is added.
        let mut core = JamCore::new("abcdefghij", member(G1, 10), Queue::new(), None, now);
        assert_eq!(core.phase, Phase::Stopped);
        core.push(entries(&["x"]), 100, false, now).unwrap();
        assert!(matches!(core.phase, Phase::Preparing { .. }));
    }

    /// "Play next" from any member goes right after the shared current track, the timeline of the
    /// playing track is untouched, and the host server's limit still applies.
    #[test]
    fn play_next_goes_after_the_shared_current_track() {
        let now = 1_000_000;
        let mut core = playing_jam(now);
        core.settings.allow_servers = true;
        core.join(member(G2, 20), now).unwrap();
        let mut rng = fastrand::Rng::with_seed(1);
        let mut enqueue = |core: &mut JamCore, from: SessionKey, ids: &[&str], max: usize, next: bool| {
            let (tx, mut rx) = oneshot::channel();
            let cmd = Command::Enqueue {
                entries: entries(ids),
                max,
                next,
                text_channel: None,
                reply: tx,
            };
            core.edit_from(from, cmd, &mut rng, now);
            rx.try_recv().unwrap()
        };
        let upcoming = |core: &JamCore| -> Vec<String> {
            core.queue
                .upcoming()
                .iter()
                .map(|e| e.info.youtube_id.clone().unwrap())
                .collect()
        };
        let (seq, phase, version) = (core.seq, core.phase, core.queue_version);
        let r = enqueue(&mut core, key(G2, 20), &["x"], 100, true).unwrap();
        assert_eq!(r.first_position, 1);
        assert_eq!(upcoming(&core), vec!["x", "b", "c"]);
        enqueue(&mut core, key(G1, 10), &["p1", "p2"], 100, true).unwrap();
        assert_eq!(upcoming(&core), vec!["p1", "p2", "x", "b", "c"]);
        enqueue(&mut core, key(G1, 10), &["z"], 100, false).unwrap();
        assert_eq!(upcoming(&core).last().unwrap(), "z");
        assert_eq!(core.queue.current().unwrap().info.youtube_id.as_deref(), Some("a"));
        assert_eq!((core.seq, core.phase), (seq, phase), "the playing track goes on");
        assert!(core.queue_version > version, "members see the new order");
        // The host server's limit (reported with its own enqueues: 100 above) applies to "next" too.
        handle(
            &mut core,
            JamCmd::HostLimit {
                key: key(G1, 10),
                max: 7,
            },
            &mut fastrand::Rng::with_seed(2),
        );
        let r = enqueue(&mut core, key(G2, 20), &["n1", "n2", "n3"], 100, true).unwrap();
        assert_eq!((r.added, r.truncated), (1, true));
        assert_eq!(upcoming(&core)[0], "n1");
        assert_eq!(
            enqueue(&mut core, key(G2, 20), &["n4"], 100, true),
            Err(PushError::Full { max: 7 })
        );
    }

    #[test]
    fn join_rules_follow_the_settings() {
        let mut core = playing_jam(0);
        // Same server: allowed by default. Other server: off by default.
        assert_eq!(core.can_join(key(G1, 11)), Ok(()));
        assert_eq!(core.can_join(key(G2, 20)), Err(JoinRefused::ServersOff));
        assert_eq!(core.can_join(key(G1, 10)), Err(JoinRefused::AlreadyIn));
        core.settings.apply(SettingsPatch {
            allow_servers: Some(true),
            allow_channels: Some(false),
            ..Default::default()
        });
        assert_eq!(core.can_join(key(G2, 20)), Ok(()));
        assert_eq!(core.can_join(key(G1, 11)), Err(JoinRefused::ChannelsOff));
        core.settings.apply(SettingsPatch {
            closed: Some(true),
            ..Default::default()
        });
        assert_eq!(core.join(member(G2, 20), 0), Err(JoinRefused::Closed));
        core.settings.closed = false;
        for c in 0..(MAX_MEMBERS as u64 - 1) {
            core.join(member(G2, 100 + c), 0).unwrap();
        }
        assert_eq!(core.can_join(key(G2, 999)), Err(JoinRefused::Full));
        core.end();
        assert_eq!(core.can_join(key(G2, 999)), Err(JoinRefused::Ended));
    }

    #[test]
    fn host_transfer_and_end() {
        let mut core = playing_jam(0);
        core.join(member(G1, 11), 0).unwrap();
        core.join(member(G1, 12), 0).unwrap();
        // 11 joined first but is empty: 12 becomes the host.
        core.set_humans(key(G1, 11), false);
        let out = core.leave(key(G1, 10), 0).unwrap();
        assert_eq!(out, LeaveOutcome { host_changed: true, ended: false });
        assert_eq!(core.host, key(G1, 12));
        // A member leaving is not a host change.
        let out = core.leave(key(G1, 11), 0).unwrap();
        assert_eq!(out, LeaveOutcome::default());
        assert!(core.leave(key(G1, 11), 0).is_none(), "already gone");
        // The last one leaving ends it.
        assert!(core.leave(key(G1, 12), 0).unwrap().ended);
        assert!(core.ended);

        // Host leaves and nobody else has people: the jam ends.
        let mut core = playing_jam(0);
        core.join(member(G1, 11), 0).unwrap();
        core.set_humans(key(G1, 11), false);
        assert!(core.leave(key(G1, 10), 0).unwrap().ended);

        // Earliest joined wins among channels with people.
        let mut core = playing_jam(0);
        core.join(member(G1, 12), 0).unwrap();
        core.join(member(G1, 11), 0).unwrap();
        core.leave(key(G1, 10), 0);
        assert_eq!(core.host, key(G1, 12));
        // The host cannot be kicked; others can.
        assert!(!core.kick(key(G1, 12), 0));
        assert!(core.kick(key(G1, 11), 0));
        assert_eq!(core.members.len(), 1);
    }

    #[test]
    fn edit_permissions() {
        let mut core = playing_jam(0);
        core.join(member(G1, 11), 0).unwrap();
        assert!(core.may_edit(key(G1, 10)));
        assert!(core.may_edit(key(G1, 11)));
        assert!(!core.may_edit(key(G1, 99)), "not a member");
        core.settings.editable_by = EditRule::Host;
        assert!(core.may_edit(key(G1, 10)));
        assert!(!core.may_edit(key(G1, 11)));
        let view = core.view();
        assert!(!view.may_edit(key(G1, 11)));

        // A refused edit answers "nothing changed" and the queue stays.
        let (tx, mut rx) = oneshot::channel();
        JamCore::refuse(Command::Clear { reply: tx });
        assert_eq!(rx.try_recv(), Ok(0));
    }

    #[test]
    fn enqueues_follow_the_host_servers_limit_and_refusals_say_why() {
        let now = 1_000_000;
        let mut core = playing_jam(now);
        core.settings.allow_servers = true;
        core.join(member(G2, 20), now).unwrap();
        let mut rng = fastrand::Rng::with_seed(1);
        let mut enqueue = |core: &mut JamCore, from: SessionKey, ids: &[&str], max: usize| {
            let (tx, mut rx) = oneshot::channel();
            let cmd = Command::Enqueue {
                entries: entries(ids),
                max,
                next: false,
                text_channel: None,
                reply: tx,
            };
            core.edit_from(from, cmd, &mut rng, now);
            rx.try_recv().unwrap()
        };
        // The host server allows 3 upcoming songs (b, c already there); it reports that limit.
        handle(
            &mut core,
            JamCmd::HostLimit {
                key: key(G1, 10),
                max: 3,
            },
            &mut fastrand::Rng::with_seed(2),
        );
        // A member of another server with a 100-song limit still hits the host's 3.
        let r = enqueue(&mut core, key(G2, 20), &["d", "e", "f"], 100).unwrap();
        assert_eq!(r.added, 1, "{r:?}");
        assert_eq!(
            enqueue(&mut core, key(G2, 20), &["g"], 100),
            Err(PushError::Full { max: 3 })
        );
        // Only the host channel reports the limit.
        handle(
            &mut core,
            JamCmd::HostLimit {
                key: key(G2, 20),
                max: 100,
            },
            &mut fastrand::Rng::with_seed(2),
        );
        assert_eq!(core.host_max_queue, Some(3));

        // A refused enqueue says "not allowed", not "the queue is full (0)".
        let (tx, mut rx) = oneshot::channel();
        JamCore::refuse(Command::Enqueue {
            entries: entries(&["x"]),
            max: 100,
            next: false,
            text_channel: None,
            reply: tx,
        });
        assert_eq!(rx.try_recv().unwrap(), Err(PushError::NotAllowed));
    }

    #[test]
    fn a_shared_skip_for_another_track_is_outdated() {
        let now = 1_000_000;
        let mut core = playing_jam(now);
        let mut rng = fastrand::Rng::with_seed(1);
        let skip = |expected| {
            let (tx, rx) = oneshot::channel();
            let cmd = Command::Skip {
                user: 1,
                force: true,
                rule: Default::default(),
                expected,
                reply: tx,
            };
            (cmd, rx)
        };
        let a = core.queue.current().unwrap().uid;
        let (cmd, mut rx) = skip(Some(uuid::Uuid::new_v4()));
        core.edit(cmd, &mut rng, now);
        assert!(matches!(rx.try_recv(), Ok(SkipOutcome::Outdated)));
        assert_eq!(core.queue.current().unwrap().uid, a);
        // The right track: skipped once; the same click again is outdated.
        for expect_skip in [true, false] {
            let (cmd, mut rx) = skip(Some(a));
            core.edit(cmd, &mut rng, now);
            let out = rx.try_recv().unwrap();
            assert_eq!(matches!(out, SkipOutcome::Skipped { .. }), expect_skip, "{out:?}");
        }
        assert_eq!(core.queue.current().unwrap().info.youtube_id.as_deref(), Some("b"));
    }

    #[test]
    fn members_start_together_after_everyone_is_ready() {
        let now = 50_000;
        let mut core = playing_jam(0);
        core.settings.allow_servers = true;
        core.join(member(G1, 11), now).unwrap();
        core.join(member(G2, 20), now).unwrap();
        // Skip: new track, everyone prepares it.
        assert!(matches!(core.skip(now), SkipOutcome::Skipped { .. }));
        let seq = core.seq;
        assert_eq!(core.phase, Phase::Preparing { since_ms: now, paused: false });
        assert!(!core.ready(key(G1, 10), seq, now + 100));
        assert!(!core.ready(key(G1, 11), seq - 1, now + 100), "stale report");
        assert!(!core.ready(key(G1, 11), seq, now + 200));
        // The last one: everyone starts at the same anchor shortly after.
        assert!(core.ready(key(G2, 20), seq, now + 300));
        assert_eq!(
            core.phase,
            Phase::Running {
                anchor_ms: now + 300 + START_DELAY_MS
            }
        );
        assert_eq!(core.position(now + 300), 0);

        // One member never answers: the wait runs out and the others start.
        core.skip(now);
        let seq = core.seq;
        core.ready(key(G1, 10), seq, now);
        core.ready(key(G1, 11), seq, now);
        assert!(!core.prepare_timeout(now + 1000));
        let late = now + PREPARE_WAIT.as_millis() as i64;
        assert!(core.prepare_timeout(late));
        assert!(matches!(core.phase, Phase::Running { .. }));
    }

    #[test]
    fn failures_and_track_ends() {
        let now = 0;
        let mut core = playing_jam(now);
        core.join(member(G1, 11), now).unwrap();
        core.skip(now); // now on b
        let seq = core.seq;
        // One member fails, the other is ready: the track plays for the one that can.
        core.failed_track(key(G1, 11), seq, now);
        assert!(core.ready(key(G1, 10), seq, now));
        assert!(matches!(core.phase, Phase::Running { .. }));
        // Both fail the next one: skipped.
        core.skip(now); // c
        let seq = core.seq;
        core.failed_track(key(G1, 10), seq, now);
        assert!(core.failed_track(key(G1, 11), seq, now));
        assert_eq!(core.seq, seq + 1);
        assert_eq!(core.phase, Phase::Stopped, "c was the last one");

        // The first end report moves the shared queue on; the duplicates are ignored.
        let mut core = playing_jam(now);
        let seq = core.seq;
        assert!(core.ended_track(key(G1, 10), seq, now));
        assert!(!core.ended_track(key(G1, 10), seq, now));
        assert_eq!(core.queue.current().unwrap().info.youtube_id.as_deref(), Some("b"));
        // Loop: song restarts the same track with a new seq.
        core.set_loop(LoopMode::Track);
        let seq = core.seq;
        let uid = core.queue.current().unwrap().uid;
        assert!(core.ended_track(key(G1, 10), seq, now));
        assert_eq!(core.seq, seq + 1);
        assert_eq!(core.queue.current().unwrap().uid, uid);
    }

    #[test]
    fn pause_resume_seek_keep_the_timeline() {
        let mut core = playing_jam(0);
        assert!(core.pause(5_000));
        assert_eq!(core.phase, Phase::Paused { position_ms: 15_000 });
        assert!(!core.pause(6_000));
        assert_eq!(core.seek(30_000, 7_000), SeekOutcome::Done);
        assert_eq!(core.phase, Phase::Paused { position_ms: 30_000 });
        assert!(core.resume(8_000));
        assert_eq!(core.position(8_000 + START_DELAY_MS), 30_000);
        assert_eq!(core.seek(500_000, 9_000), SeekOutcome::OutOfRange);
        assert_eq!(core.seek(60_000, 9_000), SeekOutcome::Done);
        assert_eq!(core.position(10_000), 61_000);
        // A pause carries over to the next track.
        core.pause(10_000);
        core.skip(10_000);
        assert!(matches!(core.phase, Phase::Preparing { paused: true, .. }));
        let seq = core.seq;
        core.ready(key(G1, 10), seq, 10_100);
        assert_eq!(core.phase, Phase::Paused { position_ms: 0 });
    }

    #[test]
    fn view_reuses_the_track_list_until_it_changes() {
        let mut core = playing_jam(0);
        let a = core.view();
        core.pause(0);
        let b = core.view();
        assert!(Arc::ptr_eq(&a.tracks, &b.tracks), "pause does not copy the queue");
        assert!(b.rev > a.rev);
        core.push(entries(&["d"]), 100, false, 0).unwrap();
        let c = core.view();
        assert_eq!(c.tracks.len(), 4);
        assert!(c.queue_version > b.queue_version);
        assert_eq!(c.current, core.queue.current().map(|e| e.uid));
        assert!(c.share_link("42").ends_with("/activities/42?custom_id=jam-abcdefghij"));
    }

    #[test]
    fn drift_decision() {
        assert_eq!(drift_correction(10_000, 10_000, Some(180_000)), None);
        assert_eq!(drift_correction(10_000, 11_400, Some(180_000)), None);
        assert_eq!(drift_correction(10_000, 11_600, Some(180_000)), Some(11_600));
        assert_eq!(drift_correction(20_000, 10_000, None), Some(10_000));
        // Close to the end: let it end.
        assert_eq!(drift_correction(170_000, 178_000, Some(180_000)), None);
    }

    #[test]
    fn ids() {
        let id = new_id();
        assert_eq!(id.len(), ID_LEN);
        assert!(id.bytes().all(|b| ID_ALPHABET.contains(&b)));
        assert_eq!(parse_id(&id), Some(id.as_str()));
        assert_eq!(parse_id(&format!("jam-{id}")), Some(id.as_str()));
        assert_eq!(parse_id("jam-partyroom1"), Some("partyroom1"), "older 10-character ids");
        assert_eq!(parse_id("jam-short"), None);
        assert_eq!(parse_id("ABCDEFGHIJ"), None);
        assert_ne!(new_id(), new_id());
        // Every byte maps into the alphabet and all 32 letters are reachable.
        let all: Vec<u8> = (0..=255).collect();
        let letters: std::collections::HashSet<char> = id_from_bytes(&all).chars().collect();
        assert_eq!(letters.len(), 32);
    }

    #[tokio::test]
    async fn actor_join_edit_and_end() {
        let jams = Jams::new();
        let mut q = Queue::new();
        q.push(entries(&["a", "b"]), 100).unwrap();
        q.start();
        let h = jams
            .create(
                member(G1, 10),
                q,
                Some(HostPosition {
                    position_ms: 0,
                    paused: false,
                }),
            )
            .unwrap();
        assert!(jams.create(member(G1, 10), Queue::new(), None).is_err(), "one jam per channel");
        assert!(jams.of(key(G1, 10)).is_some());
        h.join(member(G1, 11)).await.unwrap();
        assert_eq!(h.join(member(G1, 11)).await, Err(JoinRefused::AlreadyIn));
        let mut rx = h.subscribe();
        rx.borrow_and_update();
        assert!(jams.of(key(G1, 11)).is_some());

        // An edit from a member goes through the jam and every member sees it.
        let (tx, reply) = oneshot::channel();
        h.post(JamCmd::Edit {
            from: key(G1, 11),
            cmd: Command::Enqueue {
                entries: entries(&["c"]),
                max: 100,
                next: false,
                text_channel: None,
                reply: tx,
            },
        });
        assert!(reply.await.unwrap().is_ok());
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().tracks.len(), 3);

        // Host-only editing refuses the member.
        h.settings(SettingsPatch {
            editable_by: Some(EditRule::Host),
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(!jams.edit_allowed(key(G1, 11)));
        assert!(jams.edit_allowed(key(G1, 10)));
        assert!(jams.edit_allowed(key(G1, 99)), "outside a jam");

        assert!(h.kick(key(G1, 11)).await);
        assert!(jams.of(key(G1, 11)).is_none());
        h.end();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !rx.borrow().ended {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while jams.len() > 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(jams.of(key(G1, 10)).is_none());
    }
}
