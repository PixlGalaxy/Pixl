//! The player actor: owns one voice channel's queue and playback. Everything reaches it as a
//! [`Command`]; timers (empty channel, queue-end leave, prefetch, position ticks) are deadlines in
//! its select loop, armed only while needed.

use std::{
    collections::HashSet,
    future::pending,
    sync::{Arc, RwLock},
    time::Duration,
};

use serenity::all::GuildId;
use songbird::{
    tracks::{ControlError, TrackHandle},
    Call, Event,
};
use tokio::{
    sync::{broadcast, mpsc, watch, Mutex},
    time::{sleep_until, Instant},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::{
    checkpoint::{self, ResumePlan, CHECKPOINT_EVERY},
    jam::{
        self, CreateRefused, HostPosition, JamCmd, JamHandle, JamView, JoinRefused, Member as JamMember, Phase,
        DRIFT_CHECK_EVERY, DRIFT_CHECK_FIRST, DRIFT_TOLERANCE_MS,
    },
    lifecycle::{deadline_epoch_ms, EmptyAction, EmptyChannel, QUEUE_END_LEAVE_AFTER},
    messages::{jam_line, Messenger, MessengerDeps, NpData, OwnedNotice},
    heads::{Fetched, HeadKey},
    playback::{http_input, track_events, ytdlp_input, TrackNotifier, TrackSignal},
    prefetch::{Plan, Prefetcher, Resolved, Role, Taken, Want},
    queue::{Entry, PushError, PushResult, Queue, QueueEntry},
    radio::{self, Job, JobKind, Radio, RadioSpec, RadioTicket, StartRefused},
    state::{epoch_ms, PausedReason, PlayerEvent, PlayerSnapshot, Status},
    votes::{required_votes, VoteResult, Votes},
    Command, EndReason, PlayerDeps, PlayerManager, Restore, SeekOutcome, SkipOutcome, StartRequest,
};
use crate::{
    ai::{RadioPick, RadioRequest},
    discord::registry::BotInfo,
    procs::ProcOwner,
    sources::{HeadFrom, LateMusic, MusicMeta, Playable, PrepareTimings, SourceError},
    store::{track_key, GuildSettings, LoopMode, SessionKey, SessionRecord, MAX_VOLUME, MIN_VOLUME},
};

const PREPARE_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_FAILURES: u32 = 3;
const POSITION_TICK: Duration = Duration::from_secs(10);
const VOICE_TIMEOUT: Duration = Duration::from_secs(5);
const SEEK_TIMEOUT: Duration = Duration::from_secs(10);
/// A fresh input that is not playable after this long counts as failed (re-resolved once, then
/// the track fails) instead of leaving the channel silent.
const PLAYABLE_TIMEOUT: Duration = Duration::from_secs(30);
const HISTORY_LEN: usize = 20;
/// A play counts for the history after this much listening (or half the track).
const PLAY_COUNTS_AFTER: Duration = Duration::from_secs(30);

pub struct Spawn {
    pub manager: PlayerManager,
    pub req: StartRequest,
    pub call: Arc<Mutex<Call>>,
    pub bot: Arc<BotInfo>,
    pub generation: u64,
    pub tx: mpsc::Sender<Command>,
    pub rx: mpsc::Receiver<Command>,
    pub events: broadcast::Sender<PlayerEvent>,
    pub snapshot: Arc<RwLock<Arc<PlayerSnapshot>>>,
}

pub fn initial_snapshot(req: &StartRequest, bot_user_id: u64) -> PlayerSnapshot {
    PlayerSnapshot {
        version: super::state::next_version(),
        guild_id: req.guild_id,
        channel_id: req.voice_channel_id,
        text_channel_id: req.text_channel_id,
        bot_index: req.bot_index,
        bot_user_id,
        status: Status::Idle,
        paused_reason: None,
        empty_channel_deadline: None,
        current: None,
        position_ms: 0,
        position_at: epoch_ms(),
        upcoming: Vec::new(),
        history: Vec::new(),
        loop_mode: req.restore.as_ref().map(|r| r.loop_mode).unwrap_or_default(),
        volume: req
            .restore
            .as_ref()
            .map(|r| r.volume)
            .unwrap_or(req.settings.default_volume),
        vote_skip_enabled: req.settings.vote_skip.enabled,
        skip_votes: None,
        ai: None,
        jam: None,
    }
}

/// Position of the playing track: a base position plus the time it has been running since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrackClock {
    /// Position when `since` started.
    base_ms: u64,
    /// Running since (None while paused).
    since: Option<Instant>,
    /// Listening time before `since`.
    listened: Duration,
}

impl TrackClock {
    fn new(running: bool) -> Self {
        Self {
            base_ms: 0,
            since: running.then(Instant::now),
            listened: Duration::ZERO,
        }
    }

    fn running(&self) -> bool {
        self.since.is_some()
    }

    fn position_ms(&self) -> u64 {
        self.base_ms + self.since.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0)
    }

    fn listened(&self) -> Duration {
        self.listened + self.since.map(|s| s.elapsed()).unwrap_or_default()
    }

    fn stop(&mut self) {
        if let Some(s) = self.since.take() {
            self.base_ms += s.elapsed().as_millis() as u64;
            self.listened += s.elapsed();
        }
    }

    fn start(&mut self) {
        if self.since.is_none() {
            self.since = Some(Instant::now());
        }
    }

    /// Jump to `position_ms` (seek, restart), keeping the running / paused state.
    fn set_position(&mut self, position_ms: u64) {
        let running = self.running();
        self.stop();
        self.base_ms = position_ms;
        if running {
            self.start();
        }
    }
}

/// The track in songbird and its clock.
struct Playing {
    entry: Entry,
    /// What the input was built from (a failed seek plays it again from the target).
    playable: Playable,
    handle: TrackHandle,
    clock: TrackClock,
    /// songbird built the input (audio was reachable). An error before that gets one fresh resolve.
    became_playable: bool,
    /// Start-up timings and when the input started being built; logged (once) when playable.
    startup: Option<(PrepareTimings, Instant)>,
}

impl Playing {
    fn running(&self) -> bool {
        self.clock.running()
    }

    fn position_ms(&self) -> u64 {
        self.clock.position_ms()
    }

    fn listened(&self) -> Duration {
        self.clock.listened()
    }

    fn stop_clock(&mut self) {
        self.clock.stop();
    }

    fn start_clock(&mut self) {
        self.clock.start();
    }
}

/// The own queue of a channel while it plays a jam's shared queue (it comes back on leaving).
struct SavedQueue {
    tracks: Vec<Entry>,
    index: usize,
    loop_mode: LoopMode,
    position_ms: u64,
    paused: bool,
}

/// This channel's membership of a jam (see `jam`).
struct JamLink {
    handle: JamHandle,
    rx: watch::Receiver<Arc<JamView>>,
    /// Last view applied.
    view: Arc<JamView>,
    /// None for the channel that started the jam: the shared queue was its own and stays.
    saved: Option<SavedQueue>,
    /// Timeline `seq` this player follows.
    seq: u64,
    queue_version: u64,
    /// The track is ready but waits for the jam's start time (or the others).
    hold: bool,
    hold_until: Option<Instant>,
    drift_at: Option<Instant>,
    has_humans: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detach {
    /// This channel left (leave, moved).
    Left,
    /// The host removed it.
    Removed,
    Ended,
}

async fn jam_changed(rx: &mut Option<watch::Receiver<Arc<JamView>>>) -> bool {
    match rx {
        Some(rx) => rx.changed().await.is_ok(),
        None => pending().await,
    }
}

/// Where a seek to `position_ms` lands on a track of `duration_ms`: past the end is refused, and
/// the last second is kept so the track does not end at once.
fn seek_target(position_ms: u64, duration_ms: Option<u64>) -> Result<u64, SeekOutcome> {
    let duration = duration_ms.filter(|d| *d > 0);
    if duration.is_some_and(|d| position_ms > d) {
        return Err(SeekOutcome::OutOfRange);
    }
    Ok(duration
        .map(|d| position_ms.min(d.saturating_sub(1000)))
        .unwrap_or(position_ms))
}

/// Seek of a restored track, or a seek asked while the track was loading, applied once it is
/// playable.
struct PendingResume {
    uid: Uuid,
    /// Saved position (kept as the checkpoint until the seek is done).
    saved_ms: u64,
    target_ms: u64,
}

impl PendingResume {
    fn at(uid: Uuid, target_ms: u64) -> Self {
        Self {
            uid,
            saved_ms: target_ms,
            target_ms,
        }
    }
}

/// Failed seeks of one track (across its restarts and its re-resolve).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeekTrouble {
    uid: Uuid,
    failures: u32,
    /// Seeking was given up: the track plays where it is (unsynced in a jam) and seeks are refused.
    gave_up: bool,
}

/// What to do after the `failures`-th failed seek of a track. A failed seek ends songbird's
/// track (or leaves it hanging), so every step starts a fresh input; the steps are bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeekRecovery {
    /// Same stream, fresh input, seek to the target once it is playable.
    Restart,
    /// Resolve the stream URL again (it may be stale), then seek once it is playable.
    Reresolve,
    /// Stop seeking this track: play it from the start on a fresh input.
    GiveUp,
}

fn seek_recovery(failures: u32, reresolved: bool) -> SeekRecovery {
    match failures {
        0 | 1 => SeekRecovery::Restart,
        2 if !reresolved => SeekRecovery::Reresolve,
        _ => SeekRecovery::GiveUp,
    }
}

/// songbird drops a seek that arrives while the track is still being readied (or while another
/// seek runs) with `Dropped` ("request was replaced by another of same type"). The track itself
/// is fine: the seek just has to be sent again once it is playable.
fn seek_was_replaced(e: &ControlError) -> bool {
    matches!(e, ControlError::Dropped)
}

/// Whether a jam may seek the playing track to line it up: only once songbird can seek it (seeks
/// sent earlier are dropped) and while seeking it still works.
fn jam_may_seek(became_playable: bool, gave_up: bool) -> bool {
    became_playable && !gave_up
}

/// The id a stream is cached and its head is keyed under: the video that plays, or the page URL.
fn played_id(p: &Playable) -> &str {
    p.played_youtube_id.as_deref().unwrap_or(&p.page_url)
}

/// A prefetched track's YouTube Music metadata that arrived meanwhile: apply it before the
/// track starts, so the now-playing card has it at once.
fn settle_late_music(p: &mut Playable) {
    let Some(music) = p.late_music.as_ref().and_then(LateMusic::try_now) else {
        return;
    };
    p.late_music = None;
    if let Some(m) = music {
        p.track.music = Some(m);
    }
}

struct Player {
    manager: PlayerManager,
    deps: Arc<PlayerDeps>,
    bot: Arc<BotInfo>,
    call: Arc<Mutex<Call>>,
    guild_id: u64,
    voice_channel_id: u64,
    /// Channels this player lived in before being moved (process / dir cleanup).
    old_channels: Vec<u64>,
    generation: u64,
    tx: mpsc::Sender<Command>,
    events: broadcast::Sender<PlayerEvent>,
    slot: Arc<RwLock<Arc<PlayerSnapshot>>>,

    queue: Queue,
    volume: u16,
    settings: GuildSettings,
    started_by: u64,
    text_channel: Option<u64>,
    messenger: Messenger,

    playing: Option<Playing>,
    /// Entry being prepared to play now.
    loading: Option<Uuid>,
    /// Look-ahead prefetching of the next tracks (see `prefetch`), and the task of its running job.
    prefetch: Prefetcher,
    prefetch_task: Option<(u64, tokio::task::AbortHandle)>,
    idle_leave_at: Option<Instant>,
    next_tick: Instant,
    /// Next position checkpoint (armed only while a track is running). Every session write moves it.
    checkpoint_at: Instant,
    resume_at: Option<PendingResume>,

    user_paused: bool,
    empty: EmptyChannel,
    /// Restored queue not started yet (nobody was in the channel).
    waiting_start: bool,
    /// Post "Back online" when the restored track starts.
    back_online: bool,
    humans: HashSet<u64>,
    votes: Votes,
    failures: u32,
    /// Track already re-resolved once after failing before it could play (stale or refused URL).
    stream_retry: Option<Uuid>,
    seek_trouble: Option<SeekTrouble>,
    /// The playing input must be playable by then (see `PLAYABLE_TIMEOUT`).
    playable_deadline: Option<Instant>,
    rng: fastrand::Rng,
    ending: Option<EndReason>,
    radio: Option<Radio>,
    jam: Option<JamLink>,
}

pub async fn run(s: Spawn) {
    let deps = s.manager.deps().clone();
    let messenger = Messenger::spawn(
        MessengerDeps {
            http: deps.bots.main().http.clone(),
            i18n: deps.i18n.clone(),
            ai: deps.ai.clone(),
            ai_enabled: deps.config.ai_enabled,
            voice_channel_id: s.req.voice_channel_id,
            guild_id: s.req.guild_id,
            store: deps.store.clone(),
        },
        s.req.text_channel_id,
    );
    let mut p = Player {
        manager: s.manager,
        bot: s.bot,
        call: s.call,
        guild_id: s.req.guild_id,
        voice_channel_id: s.req.voice_channel_id,
        old_channels: Vec::new(),
        generation: s.generation,
        tx: s.tx,
        events: s.events,
        slot: s.snapshot,
        queue: Queue::new(),
        volume: s.req.settings.default_volume.clamp(MIN_VOLUME, MAX_VOLUME),
        settings: s.req.settings,
        started_by: s.req.started_by,
        text_channel: s.req.text_channel_id,
        messenger,
        playing: None,
        loading: None,
        prefetch: Prefetcher::new(deps.config.prefetch_tracks),
        prefetch_task: None,
        idle_leave_at: None,
        next_tick: Instant::now() + POSITION_TICK,
        checkpoint_at: Instant::now() + CHECKPOINT_EVERY,
        resume_at: None,
        user_paused: false,
        empty: EmptyChannel::default(),
        waiting_start: false,
        back_online: false,
        humans: s.req.humans,
        votes: Votes::default(),
        failures: 0,
        stream_retry: None,
        seek_trouble: None,
        playable_deadline: None,
        rng: fastrand::Rng::new(),
        ending: None,
        radio: None,
        jam: None,
        deps,
    };
    p.boot(s.req.restore).await;
    let mut rx = s.rx;
    let reason = p.event_loop(&mut rx).await;
    p.finish(reason).await;
    // Drain so pending requests get dropped replies instead of hanging until their timeout.
    rx.close();
    while rx.try_recv().is_ok() {}
}

async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => sleep_until(d).await,
        None => pending().await,
    }
}

impl Player {
    fn key(&self) -> SessionKey {
        SessionKey {
            guild_id: self.guild_id,
            voice_channel_id: self.voice_channel_id,
        }
    }

    fn owner(&self, track: Option<Uuid>) -> ProcOwner {
        ProcOwner::new(self.guild_id, self.voice_channel_id, track)
    }

    fn paused(&self) -> bool {
        self.user_paused || self.empty_paused() || self.jam.as_ref().is_some_and(|j| j.hold)
    }

    /// The empty-channel wait pauses the music, except in a jam: there the channel keeps following
    /// the shared timeline (in sync if someone comes back) until it leaves at the deadline.
    fn empty_paused(&self) -> bool {
        self.empty.is_waiting() && self.jam.is_none()
    }

    async fn boot(&mut self, restore: Option<Restore>) {
        if let Some(r) = restore {
            let mut index = r.index;
            if let Some(e) = r.tracks.get(index) {
                match checkpoint::resume_plan(r.position_ms, e.info.duration_ms) {
                    ResumePlan::Start => {}
                    ResumePlan::SeekTo(target_ms) => {
                        self.resume_at = Some(PendingResume {
                            uid: e.uid,
                            saved_ms: r.position_ms,
                            target_ms,
                        });
                    }
                    ResumePlan::Next => {
                        info!(
                            guild = self.guild_id,
                            channel = self.voice_channel_id,
                            position_ms = r.position_ms,
                            "restored track was almost over, starting the next one"
                        );
                        index = checkpoint::next_index(index, r.tracks.len(), r.loop_mode);
                    }
                }
            }
            self.queue = super::queue::Queue::restore(r.tracks, index, r.loop_mode);
            self.volume = r.volume.clamp(MIN_VOLUME, MAX_VOLUME);
            self.back_online = r.after_restart;
            self.persist(true);
            if self.humans.is_empty() {
                self.waiting_start = self.queue.cursor().is_some();
                self.on_humans_changed().await;
            } else if self.queue.start().is_some() {
                self.play_current().await;
            } else {
                self.on_queue_end();
            }
        } else if self.humans.is_empty() {
            self.on_humans_changed().await;
        }
        self.publish();
    }

    async fn event_loop(&mut self, rx: &mut mpsc::Receiver<Command>) -> EndReason {
        loop {
            if let Some(reason) = self.ending.take() {
                return reason;
            }
            self.maybe_refill();
            let tick = (self.events.receiver_count() > 0 && self.playing.as_ref().is_some_and(|p| p.running()))
                .then_some(self.next_tick);
            // Idle and paused players have no checkpoint timer.
            let checkpoint = self
                .playing
                .as_ref()
                .is_some_and(|p| p.running())
                .then_some(self.checkpoint_at);
            // Jam timers exist only while this channel is in a jam.
            let mut jam_rx = self.jam.as_ref().map(|j| j.rx.clone());
            let jam_hold = self.jam.as_ref().and_then(|j| j.hold_until);
            let jam_drift = self.jam.as_ref().and_then(|j| j.drift_at);
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd).await,
                    None => return EndReason::GuildGone,
                },
                _ = until(self.empty.deadline()) => {
                    // Voice events can arrive out of order: check the cache before deleting the queue.
                    let humans = self.deps.bots.humans_in(self.guild_id, self.voice_channel_id);
                    if humans.is_empty() {
                        self.ending = Some(EndReason::EmptyTimeout);
                    } else {
                        self.humans = humans;
                        self.on_humans_changed().await;
                    }
                }
                _ = until(self.idle_leave_at) => self.ending = Some(EndReason::QueueEnded),
                _ = until(self.prefetch.deadline()) => self.prefetch_tick(),
                _ = until(checkpoint) => self.persist(false),
                alive = jam_changed(&mut jam_rx) => {
                    if alive {
                        self.on_jam_view().await;
                    } else {
                        self.detach(Detach::Ended, false).await;
                    }
                }
                _ = until(jam_hold) => {
                    if let Some(j) = self.jam.as_mut() {
                        j.hold_until = None;
                    }
                    self.jam_timing().await;
                }
                _ = until(jam_drift) => self.drift_check().await,
                _ = until(self.playable_deadline) => self.playable_timeout().await,
                _ = until(tick) => {
                    self.next_tick = Instant::now() + POSITION_TICK;
                    if self.playing.is_some() {
                        let _ = self.events.send(PlayerEvent::Position { position_ms: self.shown_position_ms(), position_at: epoch_ms() });
                    }
                }
            }
        }
    }

    async fn handle(&mut self, cmd: Command) {
        let cmd = if self.jam.is_some() {
            match self.jam_intercept(cmd) {
                Some(cmd) => cmd,
                None => return,
            }
        } else {
            cmd
        };
        match cmd {
            Command::Enqueue {
                entries,
                max,
                text_channel,
                reply,
            } => {
                if let Some(c) = text_channel {
                    self.set_text_channel(c);
                }
                let res = self.push_entries(entries, max).await;
                let _ = reply.send(res);
            }
            Command::Pause { reply } => {
                let changed = !self.user_paused;
                self.user_paused = true;
                self.empty.user_paused();
                self.apply_pause();
                let _ = reply.send(changed);
            }
            Command::Resume { reply } => {
                let changed = self.user_paused;
                self.user_paused = false;
                self.empty.user_resumed();
                if self.waiting_start && !self.empty.is_waiting() {
                    self.start_waiting().await;
                }
                self.apply_pause();
                let _ = reply.send(changed);
            }
            Command::Skip {
                user,
                force,
                rule,
                expected,
                reply,
            } => {
                let outcome = self.skip(user, force, &rule, expected).await;
                let _ = reply.send(outcome);
            }
            Command::Previous { reply } => {
                if self.queue.index() == 0 {
                    let _ = reply.send(None);
                    return;
                }
                self.end_track();
                let prev = self.queue.previous();
                if prev.is_some() {
                    self.play_current().await;
                }
                self.persist(false);
                let _ = reply.send(prev);
            }
            Command::Stop { by, announce } => self.ending = Some(EndReason::Stopped { by, announce }),
            Command::Seek { position_ms, reply } => {
                let outcome = self.user_seek(position_ms).await;
                let _ = reply.send(outcome);
            }
            Command::Volume { volume, reply } => {
                let prev = self.volume;
                self.volume = volume.clamp(MIN_VOLUME, MAX_VOLUME);
                if let Some(p) = &self.playing {
                    // 1.0 keeps Opus passthrough; songbird only re-encodes when the volume differs.
                    let _ = p.handle.set_volume(f32::from(self.volume) / 100.0);
                }
                self.persist(false);
                self.publish();
                let _ = reply.send(prev);
            }
            Command::Loop { mode, reply } => {
                self.queue.loop_mode = mode;
                self.prefetch.queue_changed(Instant::now());
                self.persist(false);
                self.publish();
                let _ = reply.send(());
            }
            Command::Shuffle { reply } => {
                let n = self.queue.shuffle(&mut self.rng);
                self.after_queue_edit();
                let _ = reply.send(n);
            }
            Command::Move { from, to, reply } => {
                let r = self.queue.move_track(from, to);
                if r.is_some() {
                    self.after_queue_edit();
                }
                let _ = reply.send(r);
            }
            Command::MoveUid { uid, to_index, reply } => {
                let r = self
                    .queue
                    .position_of(uid)
                    .and_then(|from| self.queue.move_track(from, to_index + 1));
                if r.is_some() {
                    self.after_queue_edit();
                }
                let _ = reply.send(r.is_some());
            }
            Command::Remove { position, reply } => {
                let r = self.queue.remove(position);
                if let Some(e) = &r {
                    self.cleanup_track(e.uid);
                    self.after_queue_edit();
                }
                let _ = reply.send(r);
            }
            Command::RemoveUid { uid, reply } => {
                let r = self.queue.remove_uid(uid);
                if let Some(e) = &r {
                    self.cleanup_track(e.uid);
                    self.after_queue_edit();
                }
                let _ = reply.send(r);
            }
            Command::SkipTo { position, reply } => {
                if position == 0 || position > self.queue.upcoming().len() {
                    let _ = reply.send(None);
                    return;
                }
                self.end_track();
                let r = self.queue.skip_to(position);
                if let Some((dropped, _)) = &r {
                    for e in dropped {
                        self.cleanup_track(e.uid);
                    }
                    self.waiting_start = false;
                    self.play_current().await;
                    self.persist(true);
                }
                let _ = reply.send(r.map(|(d, next)| (d.len(), next)));
            }
            Command::Clear { reply } => {
                let removed = self.queue.clear_upcoming();
                for e in &removed {
                    self.cleanup_track(e.uid);
                }
                self.after_queue_edit();
                let _ = reply.send(removed.len());
            }
            Command::Settings(s) => {
                self.volume = self.volume.clamp(MIN_VOLUME, MAX_VOLUME);
                self.settings = s;
                self.report_queue_limit();
                self.publish();
            }
            Command::SetTextChannel(c) => self.set_text_channel(c),
            Command::Humans(h) => {
                self.humans = h;
                self.on_humans_changed().await;
            }
            Command::Listeners => self.on_listeners_changed().await,
            Command::Moved(to) => self.moved(to).await,
            Command::End(reason) => self.ending = Some(reason),
            Command::Prepared { uid, prefetch, result } => self.on_prepared(uid, prefetch, *result).await,
            Command::PrefetchDone { job } => {
                if self.prefetch_task.as_ref().is_some_and(|(j, _)| *j == job) {
                    self.prefetch_task = None;
                }
                self.prefetch.done(job, Instant::now());
            }
            Command::MusicMeta { uid, music } => self.on_music_meta(uid, music),
            Command::Track { handle_id, signal } => self.on_track_signal(handle_id, signal).await,
            Command::StartRadio { spec, reply } => {
                let r = self.start_radio(spec);
                let _ = reply.send(r);
            }
            Command::StopRadio { reply } => {
                let stopped = self.radio.as_mut().is_some_and(Radio::stop);
                if stopped {
                    info!(guild = self.guild_id, channel = self.voice_channel_id, "radio stopped");
                    self.publish();
                }
                let _ = reply.send(stopped);
            }
            Command::CancelRadioJob { job_id, reply } => {
                let current = self.queue.current().map(|e| e.uid);
                let done = self.radio.as_mut().is_some_and(|r| r.cancel(job_id, current));
                if done {
                    self.publish();
                }
                let _ = reply.send(done);
            }
            Command::RadioPhase { id, phase } => {
                // Final phases are applied by RadioDone, after the last picks are queued.
                if phase.is_final() {
                    return;
                }
                let Some(job) = self.radio.as_mut().and_then(|r| r.job_mut(id)) else {
                    return;
                };
                if job.phase != phase {
                    job.set_phase(phase);
                    self.publish();
                }
            }
            Command::RadioPicks { id, picks } => self.radio_picks(id, picks, true).await,
            Command::JamCreate { reply } => {
                let r = self.jam_create().await;
                let _ = reply.send(r);
            }
            Command::JamJoin { jam, reply } => {
                let r = self.jam_join(jam).await;
                let _ = reply.send(r);
            }
            Command::JamLeave { reply } => {
                let was = self.jam.is_some();
                self.detach(Detach::Left, true).await;
                let _ = reply.send(was);
            }
            Command::RadioDone { id, result } => {
                let result = match *result {
                    Ok(picks) => {
                        self.radio_picks(id, picks, false).await;
                        Ok(0)
                    }
                    Err(e) => Err(e),
                };
                let current = self.queue.current().map(|e| e.uid);
                if let Some(r) = self.radio.as_mut() {
                    r.job_done(id, result, current);
                }
                // A radio that ends with nothing to play (e.g. "no songs found" on a fresh player)
                // must not keep the bot in the channel forever: arm the normal idle leave.
                if self.queue.current().is_none()
                    && self.playing.is_none()
                    && self.loading.is_none()
                    && self.idle_leave_at.is_none()
                {
                    self.idle_leave_at = Some(Instant::now() + QUEUE_END_LEAVE_AFTER);
                }
                self.publish();
            }
        }
    }

    /// Append entries (respecting `max` upcoming) and start playing when idle.
    async fn push_entries(&mut self, entries: Vec<Entry>, max: usize) -> Result<PushResult, PushError> {
        let res = self.queue.push(entries, max);
        if res.is_ok() {
            self.persist(true);
            if self.playing.is_none() && self.loading.is_none() && !self.waiting_start && !self.queue.is_active() {
                if self.queue.start().is_some() {
                    self.play_current().await;
                }
            } else {
                // New tracks (radio refills too) may be among the next ones.
                self.prefetch.queue_changed(Instant::now());
            }
            self.publish();
        }
        res
    }

    // ---- AI radio --------------------------------------------------------------------------

    fn max_queue(&self) -> usize {
        self.settings.max_queue_length as usize
    }

    fn radio_request(&self, spec: &RadioSpec, batch_size: usize) -> RadioRequest {
        let owner = spec.owner.id;
        let mut listener_ids: Vec<u64> = self.humans.iter().copied().filter(|h| *h != owner).collect();
        listener_ids.sort_unstable();
        RadioRequest {
            guild_id: self.guild_id,
            channel_id: self.voice_channel_id,
            owner_id: owner,
            listener_ids,
            prompt: spec.prompt.clone(),
            recent_keys: self.queue.all().iter().map(|e| track_key(&e.info)).collect(),
            batch_size,
            mix: spec.mix,
            blend: spec.blend,
        }
    }

    fn start_radio(&mut self, spec: RadioSpec) -> Result<RadioTicket, StartRefused> {
        if !self.deps.ai.enabled() {
            return Err(StartRefused::Disabled);
        }
        let max = self.max_queue();
        let room = max.saturating_sub(self.queue.upcoming().len());
        if room == 0 {
            return Err(StartRefused::QueueFull { max });
        }
        // A new radio replaces the old one (its job is cancelled).
        if let Some(old) = self.radio.as_mut() {
            old.stop();
        }
        let job = self
            .deps
            .ai
            .start_radio(self.radio_request(&spec, radio::START_BATCH.min(room)));
        let (state, progress) = Job::new(job.id, JobKind::Start, job.cancel_token());
        let ticket = RadioTicket {
            job_id: job.id,
            progress,
        };
        info!(
            guild = self.guild_id,
            channel = self.voice_channel_id,
            owner = spec.owner.id,
            job = %job.id,
            "radio started"
        );
        self.radio = Some(Radio::new(spec, state));
        tokio::spawn(radio::drive(job, self.tx.clone()));
        self.publish();
        Ok(ticket)
    }

    /// Start a refill when the radio needs one (one at a time).
    fn maybe_refill(&mut self) {
        let current = self.queue.current().map(|e| e.uid);
        let upcoming = self.queue.upcoming().len();
        let max = self.max_queue();
        let Some((n, spec)) = self
            .radio
            .as_ref()
            .and_then(|r| r.refill_size(upcoming, max, current).map(|n| (n, r.spec.clone())))
        else {
            return;
        };
        let job = self.deps.ai.refill(self.radio_request(&spec, n));
        let (state, _) = Job::new(job.id, JobKind::Refill, job.cancel_token());
        debug!(guild = self.guild_id, job = %job.id, picks = n, "radio refill");
        if let Some(r) = self.radio.as_mut() {
            r.hold = None;
            r.job = Some(state);
        }
        tokio::spawn(radio::drive(job, self.tx.clone()));
        self.publish();
    }

    /// Queue picks of the running job `id` (ignored once it ended or was replaced).
    async fn radio_picks(&mut self, id: Uuid, picks: Vec<RadioPick>, first: bool) {
        let Some(owner) = self.radio.as_mut().and_then(|r| {
            let owner = r.spec.owner.clone();
            r.job_mut(id).map(|_| owner)
        }) else {
            return;
        };
        if picks.is_empty() {
            return;
        }
        let count = picks.len();
        let max = self.max_queue();
        let (added, truncated) = match self.push_entries(radio::pick_entries(picks, &owner), max).await {
            Ok(r) => (r.added, r.truncated),
            Err(PushError::Full { .. } | PushError::NotAllowed) => (0, true),
        };
        if let Some(job) = self.radio.as_mut().and_then(|r| r.job_mut(id)) {
            job.added(added, truncated || added < count, first && added > 0);
        }
        self.publish();
    }

    fn set_text_channel(&mut self, c: u64) {
        if self.text_channel != Some(c) {
            self.text_channel = Some(c);
            self.messenger.set_channel(Some(c));
            self.persist(false);
        }
    }

    fn after_queue_edit(&mut self) {
        self.prefetch.queue_changed(Instant::now());
        self.persist(true);
        self.publish();
    }

    // ---- playback --------------------------------------------------------------------------

    /// Start (or prepare) the queue's current track.
    async fn play_current(&mut self) {
        self.end_track();
        let Some(entry) = self.queue.current().cloned() else {
            self.on_queue_end();
            return;
        };
        self.votes.reset();
        self.idle_leave_at = None;
        match self.prefetch.take(entry.uid, crate::store::now_secs()) {
            Taken::Fresh(playable) => {
                let mut playable = *playable;
                settle_late_music(&mut playable);
                let entry = self.adopt_prepared(entry, &playable);
                self.begin(entry, playable).await;
                return;
            }
            // The running prefetch of this track starts it (see `on_prepared`).
            Taken::Pending => {
                self.loading = Some(entry.uid);
                self.publish();
                return;
            }
            // Resolved again below; a head of the same encode stays usable.
            Taken::Stale | Taken::None => {}
        }
        self.loading = Some(entry.uid);
        self.spawn_prepare(&entry);
        self.publish();
    }

    /// Prepare `entry` to play now (its result arrives as `Command::Prepared`).
    fn spawn_prepare(&self, entry: &Entry) {
        let sources = self.deps.sources.clone();
        let owner = self.owner(Some(entry.uid));
        let tx = self.tx.clone();
        let info = entry.info.clone();
        let uid = entry.uid;
        tokio::spawn(async move {
            let result = match tokio::time::timeout(PREPARE_TIMEOUT, sources.prepare(&info, &owner)).await {
                Ok(r) => r,
                Err(_) => Err(SourceError::Timeout),
            };
            let cmd = Command::Prepared {
                uid,
                prefetch: None,
                result: Box::new(result),
            };
            let _ = tokio::time::timeout(Duration::from_secs(10), tx.send(cmd)).await;
        });
    }

    async fn on_prepared(&mut self, uid: Uuid, prefetch: Option<u64>, result: Result<Playable, SourceError>) {
        let result = match prefetch {
            None => result,
            Some(job) => match self.prefetch.resolved(job, uid, result) {
                Resolved::Stored | Resolved::Ignored => return,
                Resolved::Claimed(result) if self.loading == Some(uid) => match *result {
                    Ok(mut p) => {
                        settle_late_music(&mut p);
                        Ok(p)
                    }
                    Err(e) => {
                        // Same as a prefetch that failed earlier: prepare it normally now.
                        debug!(%uid, error = %e, "prefetch of the waiting track failed; preparing it again");
                        if let Some(entry) = self.queue.current().filter(|e| e.uid == uid).cloned() {
                            self.spawn_prepare(&entry);
                        }
                        return;
                    }
                },
                // Claimed, but another track was asked for meanwhile: keep it for later.
                Resolved::Claimed(result) => {
                    if let Ok(p) = *result {
                        self.prefetch.store(uid, p);
                    }
                    return;
                }
            },
        };
        if self.loading != Some(uid) {
            return;
        }
        self.loading = None;
        let Some(entry) = self.queue.current().cloned().filter(|e| e.uid == uid) else {
            return;
        };
        match result {
            Ok(playable) => {
                let entry = self.adopt_prepared(entry, &playable);
                self.begin(entry, playable).await;
            }
            Err(e) => {
                debug!(guild = self.guild_id, error = %e, "prepare failed");
                self.failed(entry, &e.public_reason()).await
            }
        }
    }

    /// YouTube Music metadata that arrived after its track started: show it in the queue, the
    /// saved session, the panel and the now-playing card.
    fn on_music_meta(&mut self, uid: Uuid, music: MusicMeta) {
        let Some(entry) = self.queue.all().iter().find(|e| e.uid == uid).cloned() else {
            return;
        };
        if entry.info.music.is_some() {
            return;
        }
        let mut info = entry.info.clone();
        info.music = Some(music);
        let updated = Arc::new(QueueEntry {
            info,
            ..(*entry).clone()
        });
        self.queue.update(updated.clone());
        let playing = match self.playing.as_mut().filter(|p| p.entry.uid == uid) {
            Some(p) => {
                p.playable.track.music = updated.info.music.clone();
                p.entry = updated.clone();
                true
            }
            None => false,
        };
        self.persist(true);
        self.publish();
        if playing {
            self.messenger.refresh_now_playing(self.np_data(&updated));
        }
    }

    /// Lazy Spotify match: keep the matched video in the queue (and the saved session). Also keep
    /// the YouTube Music metadata found while preparing (playlist entries).
    fn adopt_prepared(&mut self, entry: Entry, playable: &Playable) -> Entry {
        if playable.track.youtube_id == entry.info.youtube_id && playable.track.music == entry.info.music {
            return entry;
        }
        let updated = Arc::new(QueueEntry {
            info: playable.track.clone(),
            ..(*entry).clone()
        });
        self.queue.update(updated.clone());
        self.persist(true);
        updated
    }

    async fn begin(&mut self, entry: Entry, mut playable: Playable) {
        if self.resume_at.as_ref().is_some_and(|r| r.uid != entry.uid) {
            self.resume_at = None;
        }
        if self.seek_trouble.is_some_and(|t| t.uid != entry.uid) {
            self.seek_trouble = None;
        }
        let begun = Instant::now();
        // Taken here so the copy kept for seeks / retries does not hold it.
        let late_music = playable.late_music.take().and_then(|l| l.take());
        let handle = match self.start_input(&entry, &playable).await {
            Ok((h, head)) => {
                playable.timings.head = head;
                h
            }
            Err(reason) => return self.failed(entry, &reason).await,
        };
        self.playable_deadline = Some(Instant::now() + PLAYABLE_TIMEOUT);
        let paused = self.paused();
        let startup = Some((playable.timings.clone(), begun));
        self.playing = Some(Playing {
            entry: entry.clone(),
            playable,
            handle,
            clock: TrackClock::new(!paused),
            became_playable: false,
            startup,
        });
        self.prefetch.track_started(Instant::now(), !paused);
        self.persist(false);
        self.publish();
        self.messenger.now_playing(self.np_data(&entry));
        if let Some(link) = &self.jam {
            link.handle.post(JamCmd::Ready {
                key: self.key(),
                seq: link.seq,
            });
            self.jam_timing().await;
        }
        let (ai, mut info) = (self.deps.ai.clone(), entry.info.clone());
        match late_music {
            None => {
                tokio::spawn(async move { ai.enqueue_tagging(&info).await });
            }
            // The catalog key depends on the song metadata: tag once it is known.
            Some(late) => {
                let (tx, uid) = (self.tx.clone(), entry.uid);
                tokio::spawn(async move {
                    let (music, took) = late.await;
                    debug!(%uid, took_ms = took.as_millis() as u64, found = music.is_some(), "late music metadata");
                    if let Some(music) = music {
                        info.music = Some(music.clone());
                        let cmd = Command::MusicMeta { uid, music };
                        let _ = tokio::time::timeout(Duration::from_secs(10), tx.send(cmd)).await;
                    }
                    ai.enqueue_tagging(&info).await;
                });
            }
        }
        if self.back_online {
            self.back_online = false;
            self.messenger.notice(OwnedNotice::BackOnline {
                title: entry.info.title.clone(),
                url: entry.info.url.clone(),
                position: self.queue.index() + 1,
                total: self.queue.all().len(),
            });
        }
    }

    /// Build the songbird input of `playable` and play it, with the track events, volume and pause
    /// state attached. A prefetched head of the same encode (this player's, or the jam host's)
    /// serves the first bytes from disk.
    async fn start_input(&mut self, entry: &Entry, playable: &Playable) -> Result<(TrackHandle, HeadFrom), String> {
        let (input, head) = if playable.ytdlp_fallback {
            let input = ytdlp_input(
                &self.deps.sources,
                &self.deps.procs,
                playable,
                &self.owner(Some(entry.uid)),
            )
            .await
            .map_err(|e| e.to_string())?;
            (input, HeadFrom::Pipe)
        } else {
            let head = match HeadKey::of(played_id(playable), &playable.stream) {
                Some(key) => self.manager.heads().lookup(&key).await,
                None => None,
            };
            let from = if head.is_some() { HeadFrom::Disk } else { HeadFrom::Net };
            (http_input(self.deps.http.clone(), &playable.stream, head), from)
        };
        let call = self.call.clone();
        let handle = match tokio::time::timeout(VOICE_TIMEOUT, call.lock()).await {
            Ok(mut call) => call.play_only_input(input),
            Err(_) => return Err("voice connection is busy".to_owned()),
        };
        let handle_id = handle.uuid();
        for (event, signal) in track_events() {
            let notifier = TrackNotifier {
                tx: self.tx.clone(),
                handle_id,
                signal,
            };
            if handle.add_event(Event::Track(event), notifier).is_err() {
                warn!("could not attach a track event");
            }
        }
        if self.volume != 100 {
            let _ = handle.set_volume(f32::from(self.volume) / 100.0);
        }
        if self.paused() {
            let _ = handle.pause();
        }
        // songbird only readies a lazy input while the track plays: ask for it now, so a track held
        // paused (jam start, empty channel) is ready to play and to seek when it is released.
        let _ = handle.make_playable();
        Ok((handle, head))
    }

    fn np_data(&self, entry: &Entry) -> NpData {
        NpData {
            entry: entry.clone(),
            node_name: (!self.bot.is_main()).then(|| self.bot.name().to_owned()),
            jam: self.jam.as_ref().map(|j| jam_line(&j.view, self.key())),
            paused: self.paused(),
        }
    }

    /// Stop the songbird track (if any), count the play and free its resources.
    fn end_track(&mut self) {
        self.loading = None;
        self.prefetch.stopped();
        self.playable_deadline = None;
        let Some(p) = self.playing.take() else { return };
        let _ = p.handle.stop();
        self.record_play(&p);
        let next_is_same = self.queue.current().is_some_and(|e| e.uid == p.entry.uid)
            && self.queue.loop_mode == crate::store::LoopMode::Track;
        if !next_is_same {
            self.cleanup_track(p.entry.uid);
        }
    }

    /// The current track finished (`skip: false`) or was skipped: move on. In a jam the shared
    /// queue moves on instead (every member follows).
    async fn next_track(&mut self, skip: bool) {
        if let Some(link) = &self.jam {
            let (key, seq) = (self.key(), link.seq);
            link.handle.post(if skip {
                JamCmd::Skip { key, seq }
            } else {
                JamCmd::Ended { key, seq }
            });
            self.end_track();
            self.publish();
            return;
        }
        self.end_track();
        match self.queue.advance(skip) {
            Some(_) => self.play_current().await,
            None => self.on_queue_end(),
        }
        self.persist(false);
    }

    async fn failed(&mut self, entry: Entry, reason: &str) {
        if let Some(link) = &self.jam {
            // Silent until the next shared track; the jam skips only when no channel can play it.
            warn!(guild = self.guild_id, track = %entry.info.url, %reason, "jam track failed here");
            self.deps.sources.invalidate_stream(&entry.info);
            link.handle.post(JamCmd::Failed {
                key: self.key(),
                seq: link.seq,
            });
            self.messenger.notice(OwnedNotice::TrackFailed {
                title: entry.info.title.clone(),
                url: entry.info.url.clone(),
                reason: reason.trim_end_matches('.').to_owned(),
            });
            self.end_track();
            self.publish();
            return;
        }
        self.failures += 1;
        warn!(guild = self.guild_id, track = %entry.info.url, %reason, failures = self.failures, "track failed");
        self.deps.sources.invalidate_stream(&entry.info);
        self.messenger.notice(OwnedNotice::TrackFailed {
            title: entry.info.title.clone(),
            url: entry.info.url.clone(),
            reason: reason.trim_end_matches('.').to_owned(),
        });
        if self.failures >= MAX_FAILURES {
            self.ending = Some(EndReason::TooManyFailures);
            return;
        }
        Box::pin(self.next_track(true)).await;
    }

    async fn on_track_signal(&mut self, handle_id: Uuid, signal: TrackSignal) {
        if self.playing.as_ref().map(|p| p.handle.uuid()) != Some(handle_id) {
            return;
        }
        match signal {
            TrackSignal::Playable => {
                self.failures = 0;
                // songbird also reports Playable after every seek; only the first one of an input
                // starts it (at 0, whatever the clock counted while it loaded).
                let mut first = false;
                if let Some(p) = self.playing.as_mut() {
                    if !p.became_playable {
                        first = true;
                        p.became_playable = true;
                        p.clock.set_position(0);
                        self.playable_deadline = None;
                    }
                    if let Some((timings, begun)) = p.startup.take() {
                        let id = p.playable.played_youtube_id.as_deref().unwrap_or(&p.playable.page_url);
                        info!(
                            guild = self.guild_id,
                            uid = %p.entry.uid,
                            track = id,
                            prefetch = timings.prefetch,
                            "track start: {}",
                            timings.summary(begun, Instant::now())
                        );
                    }
                }
                if self.jam.is_some() {
                    // The jam timeline is the target: line up from the real position (a target kept
                    // while it loaded is stale by now).
                    let uid = self.playing.as_ref().map(|p| p.entry.uid);
                    let pending = self.resume_at.as_ref().is_some_and(|r| Some(r.uid) == uid);
                    if first || pending {
                        if pending {
                            self.resume_at = None;
                        }
                        self.jam_timing().await;
                    }
                } else {
                    self.apply_resume().await;
                }
            }
            TrackSignal::End => self.next_track(false).await,
            TrackSignal::Error => {
                // The stream URL was refused (e.g. a 403 on a prefetched URL, or an URL that went
                // stale mid-track): resolve it again once instead of failing the track, and go on
                // where it was.
                let retry = self
                    .playing
                    .as_ref()
                    .filter(|p| self.stream_retry != Some(p.entry.uid))
                    .map(|p| (p.entry.clone(), p.became_playable.then(|| p.position_ms())));
                if let Some((entry, at)) = retry {
                    match at {
                        None => {
                            warn!(guild = self.guild_id, track = %entry.info.url, "stream refused before playing; resolving it again")
                        }
                        Some(at) => {
                            warn!(guild = self.guild_id, track = %entry.info.url, position = at, "stream failed mid-track; resolving it again to go on there")
                        }
                    }
                    self.reresolve(&entry, at.filter(|ms| *ms > 0));
                    return;
                }
                let entry = self.playing.as_ref().map(|p| p.entry.clone());
                if let Some(entry) = entry {
                    self.failed(entry, "the stream stopped with an error").await;
                }
            }
        }
    }

    fn on_queue_end(&mut self) {
        self.playing = None;
        self.loading = None;
        self.prefetch.stopped();
        self.messenger.clear_now_playing();
        self.messenger.notice(OwnedNotice::QueueFinished);
        self.idle_leave_at = Some(Instant::now() + QUEUE_END_LEAVE_AFTER);
        self.publish();
    }

    fn record_play(&self, p: &Playing) {
        if !self.deps.config.ai_enabled {
            return;
        }
        let listened = p.listened();
        let half = p
            .entry
            .info
            .duration_ms
            .filter(|d| *d > 0)
            .map(|d| Duration::from_millis(d / 2));
        if listened < PLAY_COUNTS_AFTER && half.is_none_or(|h| listened < h) {
            return;
        }
        let store = self.deps.store.clone();
        let ai = self.deps.ai.clone();
        let (user, info) = (p.entry.requester.id, p.entry.info.clone());
        tokio::spawn(async move {
            match store.record_play(user, &info).await {
                Ok(Some(_)) => ai.notify_new_tracks(),
                Ok(None) => {}
                Err(e) => debug!(error = %e, "could not record a play"),
            }
        });
    }

    /// Kill the processes and remove the temp dir (with its prefetched head) of one track, in the
    /// background.
    fn cleanup_track(&self, uid: Uuid) {
        let procs = self.deps.procs.clone();
        let dirs = self.deps.sources.stream_dirs().clone();
        let (g, c) = (self.guild_id, self.voice_channel_id);
        self.manager.heads().forget_dir(&dirs.track_path(g, c, uid));
        tokio::spawn(async move {
            procs.kill_track(g, c, uid).await;
            dirs.remove_track(g, c, uid).await;
        });
    }

    /// Pause or resume songbird to match `paused()`.
    fn apply_pause(&mut self) {
        let want = self.paused();
        if let Some(p) = &mut self.playing {
            if want && p.running() {
                let _ = p.handle.pause();
                p.stop_clock();
            } else if !want && !p.running() {
                let _ = p.handle.play();
                p.start_clock();
            }
        }
        let running = self.playing.as_ref().is_some_and(Playing::running);
        self.prefetch.set_running(Instant::now(), running);
        self.persist(false);
        self.publish();
        if let Some(p) = &self.playing {
            self.messenger.refresh_now_playing(self.np_data(&p.entry));
        }
    }

    /// A seek asked by a user. While the current track is still being prepared (nothing is
    /// playing yet), the target is kept and applied once the track is playable, like a restored
    /// position.
    async fn user_seek(&mut self, position_ms: u64) -> SeekOutcome {
        if self.playing.is_some() {
            // The newest target wins over one still waiting for the track to become playable.
            self.resume_at = None;
            return self.seek(position_ms).await;
        }
        let Some(entry) = self.queue.current().filter(|e| self.loading == Some(e.uid)).cloned() else {
            return SeekOutcome::NothingPlaying;
        };
        let target = match seek_target(position_ms, entry.info.duration_ms) {
            Ok(t) => t,
            Err(outcome) => return outcome,
        };
        self.resume_at = Some(PendingResume {
            uid: entry.uid,
            saved_ms: target,
            target_ms: target,
        });
        self.persist(false);
        self.publish();
        let _ = self.events.send(PlayerEvent::Position {
            position_ms: target,
            position_at: epoch_ms(),
        });
        SeekOutcome::Done
    }

    /// Seek the playing track. Before songbird can seek it (still loading, or restarted after a
    /// failed seek) the target is kept and applied when it becomes playable: songbird drops seeks
    /// sent earlier. A failed seek starts the bounded recovery of `seek_failed`.
    async fn seek(&mut self, position_ms: u64) -> SeekOutcome {
        let Some(p) = &mut self.playing else {
            return SeekOutcome::NothingPlaying;
        };
        let target = match seek_target(position_ms, p.entry.info.duration_ms) {
            Ok(t) => t,
            Err(outcome) => return outcome,
        };
        let uid = p.entry.uid;
        if !p.became_playable {
            debug!(
                guild = self.guild_id,
                channel = self.voice_channel_id,
                target,
                "seek kept until the track is playable"
            );
            return self.seek_later(uid, target);
        }
        if self.seek_trouble.is_some_and(|t| t.uid == uid && t.gave_up) {
            debug!(
                guild = self.guild_id,
                channel = self.voice_channel_id,
                target,
                "seek refused: seeking does not work on this track"
            );
            return SeekOutcome::Failed;
        }
        let seeked = tokio::time::timeout(SEEK_TIMEOUT, p.handle.seek_async(Duration::from_millis(target))).await;
        let reason = match seeked {
            Ok(Ok(_)) => {
                p.clock.set_position(target);
                let pos = p.position_ms();
                self.persist(false);
                self.publish();
                let _ = self.events.send(PlayerEvent::Position {
                    position_ms: pos,
                    position_at: epoch_ms(),
                });
                return SeekOutcome::Done;
            }
            Ok(Err(e)) if seek_was_replaced(&e) => {
                info!(
                    guild = self.guild_id,
                    channel = self.voice_channel_id,
                    target,
                    "seek replaced while the track was busy; applying it when it is playable"
                );
                return self.seek_later(uid, target);
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => format!("no answer in {SEEK_TIMEOUT:?}"),
        };
        self.seek_failed(target, &reason).await
    }

    /// Keep `target` for track `uid` until it is playable (`apply_resume` / the jam line-up).
    fn seek_later(&mut self, uid: Uuid, target: u64) -> SeekOutcome {
        self.resume_at = Some(PendingResume::at(uid, target));
        if let Some(p) = self.playing.as_mut().filter(|p| p.entry.uid == uid) {
            p.clock.set_position(target);
        }
        self.persist(false);
        self.publish();
        let _ = self.events.send(PlayerEvent::Position {
            position_ms: target,
            position_at: epoch_ms(),
        });
        SeekOutcome::Done
    }

    /// A seek of the playing track failed or timed out: restart it on a fresh input at the target,
    /// then resolve the stream again, then give up seeking it (play it from the start). Every step
    /// is logged; none of them loops.
    async fn seek_failed(&mut self, target: u64, reason: &str) -> SeekOutcome {
        let Some(p) = &self.playing else {
            return SeekOutcome::NothingPlaying;
        };
        let (entry, uid) = (p.entry.clone(), p.entry.uid);
        let trouble = match self.seek_trouble.filter(|t| t.uid == uid) {
            Some(t) => SeekTrouble {
                failures: t.failures + 1,
                ..t
            },
            None => SeekTrouble {
                uid,
                failures: 1,
                gave_up: false,
            },
        };
        let step = seek_recovery(trouble.failures, self.stream_retry == Some(uid));
        self.seek_trouble = Some(SeekTrouble {
            gave_up: step == SeekRecovery::GiveUp,
            ..trouble
        });
        let (guild, channel, failures) = (self.guild_id, self.voice_channel_id, trouble.failures);
        match step {
            SeekRecovery::Restart => {
                warn!(guild, channel, track = %entry.info.url, target, failures, %reason, "seek failed; restarting the track on a fresh input at the target");
                self.restart_input(Some(target)).await
            }
            SeekRecovery::Reresolve => {
                warn!(guild, channel, track = %entry.info.url, target, failures, %reason, "seek failed again; resolving the stream again");
                self.reresolve(&entry, Some(target));
                SeekOutcome::Done
            }
            SeekRecovery::GiveUp => {
                if self.jam.is_some() {
                    warn!(guild, channel, track = %entry.info.url, target, failures, %reason, "seeking does not work on this track; playing it from the start, out of sync with the jam");
                } else {
                    warn!(guild, channel, track = %entry.info.url, target, failures, %reason, "seeking does not work on this track; playing it from the start");
                }
                match self.restart_input(None).await {
                    SeekOutcome::Done => SeekOutcome::Failed,
                    outcome => outcome,
                }
            }
        }
    }

    /// Play the current track again on a fresh input built from the same stream (songbird ends or
    /// abandons a track whose seek failed), going to `target` once it is playable. Events of the
    /// old handle are ignored from here on (they no longer match the current handle).
    async fn restart_input(&mut self, target: Option<u64>) -> SeekOutcome {
        let Some(p) = &self.playing else {
            return SeekOutcome::NothingPlaying;
        };
        let (entry, playable) = (p.entry.clone(), p.playable.clone());
        let _ = p.handle.stop();
        let handle = match self.start_input(&entry, &playable).await {
            Ok((h, _)) => h,
            Err(reason) => {
                warn!(guild = self.guild_id, channel = self.voice_channel_id, track = %entry.info.url, %reason, "could not restart the track");
                self.failed(entry, &reason).await;
                return SeekOutcome::Failed;
            }
        };
        info!(guild = self.guild_id, channel = self.voice_channel_id, track = %entry.info.url, ?target, "track restarted on a fresh input");
        self.playable_deadline = Some(Instant::now() + PLAYABLE_TIMEOUT);
        if let Some(p) = &mut self.playing {
            p.handle = handle;
            p.became_playable = false;
            p.clock.set_position(target.unwrap_or(0));
        }
        self.resume_at = target.map(|t| PendingResume::at(entry.uid, t));
        self.persist(false);
        self.publish();
        let _ = self.events.send(PlayerEvent::Position {
            position_ms: target.unwrap_or(0),
            position_at: epoch_ms(),
        });
        SeekOutcome::Done
    }

    /// Drop the playing input, resolve the stream of `entry` again (once per track: a second
    /// failure fails the track) and start it, at `target` once playable.
    fn reresolve(&mut self, entry: &Entry, target: Option<u64>) {
        self.stream_retry = Some(entry.uid);
        self.deps.sources.invalidate_stream(&entry.info);
        self.end_track();
        self.loading = Some(entry.uid);
        // Without a new target, a position still kept for it (restored checkpoint) stays.
        if let Some(t) = target {
            self.resume_at = Some(PendingResume::at(entry.uid, t));
        }
        self.spawn_prepare(entry);
        self.publish();
    }

    /// The playing input did not become playable in time: handle it like a stream error.
    async fn playable_timeout(&mut self) {
        self.playable_deadline = None;
        let Some(p) = self.playing.as_ref().filter(|p| !p.became_playable) else {
            return;
        };
        warn!(
            guild = self.guild_id,
            channel = self.voice_channel_id,
            track = %p.entry.info.url,
            "track did not become playable in {PLAYABLE_TIMEOUT:?}"
        );
        let id = p.handle.uuid();
        let _ = p.handle.stop();
        self.on_track_signal(id, TrackSignal::Error).await;
    }

    /// The track is playable: go to the position kept for it (restored checkpoint, a seek asked
    /// while it loaded, or a seek retried on a fresh input).
    async fn apply_resume(&mut self) {
        let playing = self.playing.as_ref().map(|p| p.entry.uid);
        if playing.is_none() || self.resume_at.as_ref().map(|r| r.uid) != playing {
            return;
        }
        let Some(r) = self.resume_at.take() else { return };
        let outcome = self.seek(r.target_ms).await;
        let applied = outcome == SeekOutcome::Done
            && self.resume_at.is_none()
            && self.playing.as_ref().is_some_and(|p| p.became_playable);
        if applied {
            info!(
                guild = self.guild_id,
                channel = self.voice_channel_id,
                target = r.target_ms,
                "resumed at the kept position"
            );
        } else if outcome != SeekOutcome::Done {
            // A failed seek already restarted the track from the start (see `seek_failed`).
            warn!(
                guild = self.guild_id,
                channel = self.voice_channel_id,
                target = r.target_ms,
                ?outcome,
                "could not resume at the kept position"
            );
            self.persist(false);
        }
    }

    async fn skip(
        &mut self,
        user: u64,
        force: bool,
        rule: &crate::store::VoteSkip,
        expected: Option<Uuid>,
    ) -> SkipOutcome {
        let Some(current) = self.queue.current().cloned() else {
            return SkipOutcome::NothingPlaying;
        };
        if stale(expected, Some(current.uid)) {
            return SkipOutcome::Outdated;
        }
        if !force && rule.enabled {
            if let Some(outcome) = self.vote_skip(user, rule) {
                return outcome;
            }
        }
        self.next_track(true).await;
        SkipOutcome::Skipped {
            skipped: current,
            next: self.queue.current().cloned(),
        }
    }

    /// Count a vote to skip; None when it passed (skip now), else the answer.
    fn vote_skip(&mut self, user: u64, rule: &crate::store::VoteSkip) -> Option<SkipOutcome> {
        let listeners = self.listeners();
        match self.votes.vote(user, &listeners, rule) {
            VoteResult::Passed => None,
            VoteResult::Counted { current, required } => {
                self.publish();
                Some(SkipOutcome::Voted { current, required })
            }
            VoteResult::AlreadyVoted { current, required } => Some(SkipOutcome::AlreadyVoted { current, required }),
            VoteResult::NotEligible => Some(SkipOutcome::NotEligible),
        }
    }

    async fn start_waiting(&mut self) {
        self.waiting_start = false;
        if self.queue.start().is_some() {
            self.play_current().await;
        } else {
            self.on_queue_end();
        }
    }

    /// People in the channel who can hear the music (deafened users don't vote or count for votes).
    fn listeners(&self) -> std::collections::HashSet<u64> {
        self.deps.bots.listeners_in(self.guild_id, self.voice_channel_id)
    }

    /// Deafen / undeafen: votes of people who can no longer hear stop counting, and fewer listeners
    /// can make the remaining votes enough.
    async fn on_listeners_changed(&mut self) {
        let rule = self.settings.vote_skip;
        if rule.enabled && self.playing.is_some() && self.votes.passes(&self.listeners(), &rule) {
            self.next_track(true).await;
        } else {
            self.publish();
        }
    }

    async fn on_humans_changed(&mut self) {
        // People leaving can make the existing votes enough.
        let rule = self.settings.vote_skip;
        if rule.enabled && self.playing.is_some() && self.votes.passes(&self.listeners(), &rule) {
            self.next_track(true).await;
        }
        let timeout = Duration::from_secs(self.deps.config.empty_channel_timeout_secs.max(1));
        match self
            .empty
            .update(self.humans.len(), !self.user_paused, Instant::now(), timeout)
        {
            EmptyAction::Nothing => {}
            EmptyAction::Pause { deadline } => {
                self.apply_pause();
                let deadline_secs = deadline_epoch_ms(deadline) / 1000;
                if self.jam.is_some() {
                    info!(
                        guild = self.guild_id,
                        channel = self.voice_channel_id,
                        "channel empty, leaving the jam at the deadline"
                    );
                    self.messenger.notice(OwnedNotice::JamEmpty { deadline_secs });
                } else {
                    info!(
                        guild = self.guild_id,
                        channel = self.voice_channel_id,
                        "channel empty, pausing"
                    );
                    self.messenger.notice(OwnedNotice::PausedEmpty { deadline_secs });
                }
            }
            EmptyAction::Resume { .. } if self.jam.is_some() => {
                // The music never stopped here: just replace the "leaving" warning.
                self.messenger.notice(OwnedNotice::JamBack);
                self.apply_pause();
            }
            EmptyAction::Resume { play } => {
                if self.waiting_start && play {
                    // Restored after a restart: "Back online" is posted when it starts.
                    self.start_waiting().await;
                } else if let Some(cur) = self.queue.current().cloned() {
                    if play {
                        self.messenger.notice(OwnedNotice::Resumed {
                            title: cur.info.title.clone(),
                            url: cur.info.url.clone(),
                        });
                    }
                }
                self.apply_pause();
            }
        }
        let key = self.key();
        if let Some(link) = self.jam.as_mut() {
            let has = !self.humans.is_empty();
            if link.has_humans != has {
                link.has_humans = has;
                link.handle.post(JamCmd::Humans { key, has });
            }
            // People came back: line up with the jam again.
            self.jam_timing().await;
        }
        self.publish();
    }

    async fn moved(&mut self, to: u64) {
        if to == self.voice_channel_id {
            return;
        }
        // A dragged bot leaves its jam (the jam is about the channel, not the bot).
        self.detach(Detach::Left, true).await;
        info!(
            guild = self.guild_id,
            from = self.voice_channel_id,
            to,
            "bot moved, following"
        );
        self.deps.sessions.delete(self.key());
        self.old_channels.push(self.voice_channel_id);
        self.voice_channel_id = to;
        self.messenger.close(true).await;
        self.messenger = Messenger::spawn(
            MessengerDeps {
                http: self.deps.bots.main().http.clone(),
                i18n: self.deps.i18n.clone(),
                ai: self.deps.ai.clone(),
                ai_enabled: self.deps.config.ai_enabled,
                voice_channel_id: to,
                guild_id: self.guild_id,
                store: self.deps.store.clone(),
            },
            self.text_channel,
        );
        self.votes.reset();
        if let Some(p) = &self.playing {
            self.messenger.now_playing(self.np_data(&p.entry));
        }
        self.persist(true);
        self.publish();
    }

    // ---- jam -------------------------------------------------------------------------------

    fn jam_member(&self) -> JamMember {
        JamMember::new(
            self.key(),
            self.deps
                .bots
                .channel_name(self.guild_id, self.voice_channel_id)
                .unwrap_or_default(),
            self.deps.bots.guild_name(self.guild_id).unwrap_or_default(),
            !self.humans.is_empty(),
        )
    }

    /// Shared-queue commands go to the jam (their replies come from it); a skip vote is counted
    /// here first, with this channel's listeners. Volume, stop and the rest stay local.
    fn jam_intercept(&mut self, cmd: Command) -> Option<Command> {
        let handle = self.jam.as_ref()?.handle.clone();
        let from = self.key();
        match cmd {
            Command::Enqueue {
                entries,
                max,
                text_channel,
                reply,
            } => {
                if let Some(c) = text_channel {
                    self.set_text_channel(c);
                }
                let cmd = Command::Enqueue {
                    entries,
                    max,
                    text_channel: None,
                    reply,
                };
                handle.post(JamCmd::Edit { from, cmd });
                None
            }
            Command::Skip {
                user,
                force,
                rule,
                expected,
                reply,
            } => {
                // The mirrored queue keeps the jam's uids: a stale request stops here.
                let current = self.queue.current().map(|e| e.uid);
                if stale(expected, current) {
                    let _ = reply.send(SkipOutcome::Outdated);
                    return None;
                }
                if !force && rule.enabled {
                    if let Some(outcome) = self.vote_skip(user, &rule) {
                        let _ = reply.send(outcome);
                        return None;
                    }
                }
                // The jam checks `expected` again against its own current track.
                let cmd = Command::Skip {
                    user,
                    force: true,
                    rule,
                    expected,
                    reply,
                };
                handle.post(JamCmd::Edit { from, cmd });
                None
            }
            cmd @ (Command::Pause { .. }
            | Command::Resume { .. }
            | Command::Previous { .. }
            | Command::Seek { .. }
            | Command::Loop { .. }
            | Command::Shuffle { .. }
            | Command::Move { .. }
            | Command::MoveUid { .. }
            | Command::Remove { .. }
            | Command::RemoveUid { .. }
            | Command::SkipTo { .. }
            | Command::Clear { .. }) => {
                handle.post(JamCmd::Edit { from, cmd });
                None
            }
            Command::StartRadio { reply, .. } => {
                let _ = reply.send(Err(StartRefused::InJam));
                None
            }
            other => Some(other),
        }
    }

    /// Host a jam: the shared queue starts as this channel's queue and the playing track goes on.
    async fn jam_create(&mut self) -> Result<JamHandle, CreateRefused> {
        if self.jam.is_some() {
            return Err(CreateRefused::AlreadyInJam);
        }
        let host_position = self
            .playing
            .as_ref()
            .filter(|p| self.queue.current().is_some_and(|c| c.uid == p.entry.uid))
            .map(|p| HostPosition {
                position_ms: p.position_ms(),
                paused: self.user_paused,
            });
        let handle = self
            .manager
            .jams()
            .create(self.jam_member(), self.queue.clone(), host_position)?;
        if let Some(r) = self.radio.as_mut() {
            r.stop();
        }
        let mut rx = handle.subscribe();
        let view = rx.borrow_and_update().clone();
        self.jam = Some(JamLink {
            handle: handle.clone(),
            rx,
            view: view.clone(),
            saved: None,
            // The playing track already is the jam's first one.
            seq: if host_position.is_some() { view.seq } else { 0 },
            queue_version: 0,
            hold: false,
            hold_until: None,
            drift_at: None,
            has_humans: !self.humans.is_empty(),
        });
        self.report_queue_limit();
        self.apply_jam_view(&view).await;
        if let Some(p) = &self.playing {
            self.messenger.refresh_now_playing(self.np_data(&p.entry));
        }
        Ok(handle)
    }

    /// Tell the jam this server's queue limit (it only listens to the host channel's player).
    fn report_queue_limit(&self) {
        if let Some(link) = &self.jam {
            link.handle.post(JamCmd::HostLimit {
                key: self.key(),
                max: self.max_queue(),
            });
        }
    }

    /// Join `jam`: save the own queue and follow the shared one.
    async fn jam_join(&mut self, jam: JamHandle) -> Result<(), JoinRefused> {
        if self.jam.is_some() {
            return Err(JoinRefused::InAnotherJam);
        }
        jam.join(self.jam_member()).await?;
        let saved = SavedQueue {
            tracks: self.queue.all().to_vec(),
            index: self.queue.index(),
            loop_mode: self.queue.loop_mode,
            position_ms: self.checkpoint_ms(),
            paused: self.user_paused,
        };
        if let Some(r) = self.radio.as_mut() {
            r.stop();
        }
        self.end_track();
        // The host prefetches the shared queue; the own prefetches are of no use there.
        let plan = self.prefetch.clear();
        self.apply_prefetch_plan(plan);
        self.idle_leave_at = None;
        self.resume_at = None;
        self.waiting_start = false;
        self.votes.reset();
        let mut rx = jam.subscribe();
        let view = rx.borrow_and_update().clone();
        info!(
            guild = self.guild_id,
            channel = self.voice_channel_id,
            jam = %jam.id,
            "joined a jam"
        );
        self.jam = Some(JamLink {
            handle: jam,
            rx,
            view: view.clone(),
            saved: Some(saved),
            seq: 0,
            queue_version: 0,
            hold: true,
            hold_until: None,
            drift_at: None,
            has_humans: !self.humans.is_empty(),
        });
        // The own queue as it was (later writes keep it while the jam lasts).
        self.persist(true);
        self.apply_jam_view(&view).await;
        let host = view.host_member().map(|m| m.name.clone()).unwrap_or_default();
        self.messenger.notice(OwnedNotice::JamEntered { host });
        Ok(())
    }

    /// A new view of the jam: leave when it ended or removed this channel, else follow it.
    async fn on_jam_view(&mut self) {
        let me = self.key();
        let (view, prev) = {
            let Some(link) = self.jam.as_mut() else { return };
            let view = link.rx.borrow_and_update().clone();
            let prev = std::mem::replace(&mut link.view, view.clone());
            (view, prev)
        };
        if view.ended {
            self.detach(Detach::Ended, false).await;
            return;
        }
        if !view.is_member(me) {
            // Right after joining the first view may not list this channel yet.
            if prev.is_member(me) {
                self.detach(Detach::Removed, false).await;
            }
            return;
        }
        self.jam_notices(&prev, &view, me);
        self.apply_jam_view(&view).await;
    }

    fn jam_notices(&mut self, prev: &JamView, view: &JamView, me: SessionKey) {
        let count = view.members.len();
        if prev.host != view.host {
            // Only the host prefetches the shared queue.
            self.prefetch.queue_changed(Instant::now());
            let channel = view.host_member().map(|m| m.name.clone()).unwrap_or_default();
            self.messenger.notice(OwnedNotice::JamHost {
                channel,
                own: view.host == me,
            });
        } else if prev.is_member(me) {
            for m in view.members.iter().filter(|m| m.key != me && !prev.is_member(m.key)) {
                self.messenger.notice(OwnedNotice::JamJoined {
                    channel: m.name.clone(),
                    count,
                });
            }
            for m in prev.members.iter().filter(|m| m.key != me && !view.is_member(m.key)) {
                self.messenger.notice(OwnedNotice::JamLeft {
                    channel: m.name.clone(),
                    count,
                });
            }
        }
        if prev.members.len() != count || prev.host != view.host {
            if let Some(p) = &self.playing {
                self.messenger.refresh_now_playing(self.np_data(&p.entry));
            }
        }
    }

    /// Mirror the shared queue, follow the pause state and start (prepare) a new shared track.
    async fn apply_jam_view(&mut self, view: &JamView) {
        let Some(link) = self.jam.as_mut() else { return };
        let new_queue = link.queue_version != view.queue_version;
        link.queue_version = view.queue_version;
        let new_track = link.seq != view.seq;
        if new_track {
            link.seq = view.seq;
            link.hold = true;
            link.hold_until = None;
            link.drift_at = None;
        }
        if new_queue {
            self.queue = Queue::mirror(view.tracks.to_vec(), view.index, view.active, view.loop_mode);
            self.prefetch.queue_changed(Instant::now());
        }
        let paused = view.phase.paused();
        if self.user_paused != paused {
            self.user_paused = paused;
            if paused {
                self.empty.user_paused();
            } else {
                self.empty.user_resumed();
            }
        }
        if new_track {
            self.resume_at = None;
            self.waiting_start = false;
            if view.current.is_some() && self.queue.current().map(|e| e.uid) == view.current {
                self.play_current().await;
            } else {
                self.end_track();
                self.on_queue_end();
            }
        }
        self.jam_timing().await;
        self.publish();
    }

    /// Line the playing track up with the jam timeline: hold it until the start time, seek when it
    /// is too far from the jam position, and keep the drift check armed while it runs.
    async fn jam_timing(&mut self) {
        let Some(link) = self.jam.as_ref() else { return };
        if link.seq != link.view.seq {
            return;
        }
        let phase = link.view.phase;
        let Some(p) = self.playing.as_ref() else {
            if let Some(link) = self.jam.as_mut() {
                link.hold_until = None;
                link.drift_at = None;
            }
            return;
        };
        let duration = p.entry.info.duration_ms;
        let position = p.position_ms();
        let may_seek = jam_may_seek(
            p.became_playable,
            self.seek_trouble.is_some_and(|t| t.uid == p.entry.uid && t.gave_up),
        );
        let mut seek_to = None;
        let (hold, hold_until, drift_at) = match phase {
            Phase::Stopped | Phase::Preparing { .. } => (true, None, None),
            Phase::Paused { position_ms } => {
                if position.abs_diff(position_ms) > DRIFT_TOLERANCE_MS {
                    seek_to = Some(position_ms);
                }
                (false, None, None)
            }
            Phase::Running { anchor_ms } => {
                let expected = epoch_ms() - anchor_ms;
                if expected < 0 {
                    let wait = Duration::from_millis(expected.unsigned_abs());
                    (true, Some(Instant::now() + wait), None)
                } else {
                    seek_to = jam::drift_correction(position, expected as u64, duration);
                    (false, None, Some(Instant::now() + DRIFT_CHECK_FIRST))
                }
            }
        };
        if let Some(link) = self.jam.as_mut() {
            link.hold = hold;
            link.hold_until = hold_until;
            link.drift_at = drift_at;
        }
        // Not seekable yet: the line-up runs again when the track becomes playable.
        if let Some(target) = seek_to.filter(|_| may_seek) {
            debug!(
                guild = self.guild_id,
                channel = self.voice_channel_id,
                position,
                target,
                "jam: lining up"
            );
            self.seek(target).await;
        }
        self.sync_pause();
    }

    /// Compare the real songbird position with the jam position; seek when they drifted apart.
    async fn drift_check(&mut self) {
        let Some(link) = self.jam.as_mut() else { return };
        link.drift_at = None;
        let Phase::Running { anchor_ms } = link.view.phase else {
            return;
        };
        if link.seq != link.view.seq {
            return;
        }
        let gave_up = |p: &Playing| self.seek_trouble.is_some_and(|t| t.uid == p.entry.uid && t.gave_up);
        let Some(p) = self
            .playing
            .as_ref()
            .filter(|p| p.running() && jam_may_seek(p.became_playable, gave_up(p)))
        else {
            return;
        };
        let actual = match tokio::time::timeout(Duration::from_secs(1), p.handle.get_info()).await {
            Ok(Ok(info)) => info.position.as_millis() as u64,
            _ => p.position_ms(),
        };
        let expected = (epoch_ms() - anchor_ms).max(0) as u64;
        if let Some(target) = jam::drift_correction(actual, expected, p.entry.info.duration_ms) {
            debug!(
                guild = self.guild_id,
                channel = self.voice_channel_id,
                actual,
                target,
                "jam: drift, seeking"
            );
            self.seek(target).await;
        }
        if let Some(link) = self.jam.as_mut() {
            link.drift_at = Some(Instant::now() + DRIFT_CHECK_EVERY);
        }
    }

    /// Pause or resume songbird only when it differs from `paused()` (no message edit otherwise).
    fn sync_pause(&mut self) {
        let want_paused = self.paused();
        if self.playing.as_ref().is_some_and(|p| p.running() == want_paused) {
            self.apply_pause();
        }
    }

    /// Stop following the jam. A member gets its own queue back (from the saved position); the
    /// channel that started it keeps the shared queue and the playing track.
    async fn detach(&mut self, kind: Detach, notify: bool) {
        let Some(link) = self.jam.take() else { return };
        if notify {
            link.handle.post(JamCmd::Leave { key: self.key() });
        }
        info!(
            guild = self.guild_id,
            channel = self.voice_channel_id,
            jam = %link.handle.id,
            ?kind,
            "left a jam"
        );
        let restored = link.saved.is_some();
        match link.saved {
            Some(saved) => {
                self.end_track();
                let plan = self.prefetch.clear();
                self.apply_prefetch_plan(plan);
                self.prefetch.queue_changed(Instant::now());
                self.queue = Queue::restore(saved.tracks, saved.index, saved.loop_mode);
                self.user_paused = saved.paused;
                if saved.paused {
                    self.empty.user_paused();
                } else {
                    self.empty.user_resumed();
                }
                self.resume_at = None;
                if let Some(e) = self.queue.cursor() {
                    if let ResumePlan::SeekTo(target_ms) =
                        checkpoint::resume_plan(saved.position_ms, e.info.duration_ms)
                    {
                        self.resume_at = Some(PendingResume {
                            uid: e.uid,
                            saved_ms: saved.position_ms,
                            target_ms,
                        });
                    }
                }
                if self.queue.cursor().is_none() {
                    self.on_queue_end();
                } else if self.humans.is_empty() {
                    self.waiting_start = true;
                } else {
                    self.queue.start();
                    self.play_current().await;
                }
            }
            None => {
                // The host keeps the queue and its prefetches; a channel that became host-less
                // starts prefetching its own queue again.
                self.prefetch.queue_changed(Instant::now());
                self.sync_pause();
            }
        }
        self.votes.reset();
        self.messenger.notice(match kind {
            Detach::Ended => OwnedNotice::JamEnded { restored },
            Detach::Removed => OwnedNotice::JamRemoved,
            Detach::Left => OwnedNotice::JamLeftOwn { restored },
        });
        self.persist(true);
        self.publish();
        if let Some(p) = &self.playing {
            self.messenger.refresh_now_playing(self.np_data(&p.entry));
        }
    }

    // ---- prefetch --------------------------------------------------------------------------

    /// Only the jam host prefetches the shared queue (URLs and heads); members reuse them.
    fn prefetch_role(&self) -> Role {
        match &self.jam {
            None => Role::Own,
            Some(j) if j.view.host == self.key() => Role::Host,
            Some(_) => Role::Member,
        }
    }

    fn prefetch_tick(&mut self) {
        let depth = self.deps.config.prefetch_tracks;
        let wanted: Vec<Want> = self
            .queue
            .peek_ahead(depth)
            .iter()
            .map(|e| Want {
                uid: e.uid,
                duration_ms: e.info.duration_ms,
            })
            .collect();
        let left_ms = self
            .playing
            .as_ref()
            .map(|p| p.entry.info.duration_ms.unwrap_or(0).saturating_sub(p.position_ms()))
            .unwrap_or(0);
        let role = self.prefetch_role();
        let plan = self
            .prefetch
            .tick(Instant::now(), &wanted, left_ms, crate::store::now_secs(), role);
        self.apply_prefetch_plan(plan);
    }

    fn apply_prefetch_plan(&mut self, plan: Plan) {
        if let Some(job) = plan.abort {
            if let Some((_, task)) = self.prefetch_task.take_if(|(j, _)| *j == job) {
                task.abort();
            }
        }
        let current = self.playing.as_ref().map(|p| p.entry.uid);
        for uid in plan.discard {
            // Never the dir of the track that plays or loads.
            if Some(uid) != current && Some(uid) != self.loading {
                self.cleanup_track(uid);
            }
        }
        if let Some(uid) = plan.retry.filter(|u| self.loading == Some(*u)) {
            if let Some(entry) = self.queue.current().filter(|e| e.uid == uid).cloned() {
                warn!(guild = self.guild_id, %uid, "prefetch of the waiting track was lost; preparing it again");
                self.spawn_prepare(&entry);
            }
        }
        if let Some((uid, job)) = plan.start {
            match self.queue.all().iter().find(|e| e.uid == uid).cloned() {
                Some(entry) => self.spawn_prefetch(&entry, job),
                None => self.prefetch.done(job, Instant::now()),
            }
        }
    }

    /// One prefetch job: prepare the track (reported as `Command::Prepared`), then make sure the
    /// head of its stream is on disk, then `Command::PrefetchDone`.
    fn spawn_prefetch(&mut self, entry: &Entry, job: u64) {
        let sources = self.deps.sources.clone();
        let heads = self.manager.heads().clone();
        let client = self.deps.http.clone();
        let owner = self.owner(Some(entry.uid));
        let dir = self
            .deps
            .sources
            .stream_dirs()
            .track_path(self.guild_id, self.voice_channel_id, entry.uid);
        let tx = self.tx.clone();
        let info = entry.info.clone();
        let uid = entry.uid;
        let guild = self.guild_id;
        let task = tokio::spawn(async move {
            let mut result = match tokio::time::timeout(PREPARE_TIMEOUT, sources.prepare(&info, &owner)).await {
                Ok(r) => r,
                Err(_) => Err(SourceError::Timeout),
            };
            let head = match &mut result {
                Ok(p) => {
                    p.timings.prefetch = true;
                    HeadKey::of(played_id(p), &p.stream).map(|key| (key, p.stream.clone()))
                }
                Err(_) => None,
            };
            let cmd = Command::Prepared {
                uid,
                prefetch: Some(job),
                result: Box::new(result),
            };
            let _ = tokio::time::timeout(Duration::from_secs(10), tx.send(cmd)).await;
            if let Some((key, stream)) = head {
                match heads.fetch(&client, &stream, &key, &dir).await {
                    Ok(Fetched::Downloaded(h)) => debug!(guild, %uid, bytes = h.len, "prefetch: head downloaded"),
                    Ok(Fetched::Existing(_)) => debug!(guild, %uid, "prefetch: head already on disk"),
                    Ok(Fetched::Busy) => debug!(guild, %uid, "prefetch: head downloading elsewhere"),
                    Err(e) => debug!(guild, %uid, error = %e, "prefetch: head download failed; the track starts from the network"),
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(10), tx.send(Command::PrefetchDone { job })).await;
        });
        self.prefetch_task = Some((job, task.abort_handle()));
    }

    // ---- state -----------------------------------------------------------------------------

    fn snapshot(&self) -> PlayerSnapshot {
        let (current, upcoming) = if self.waiting_start {
            let up = self.queue.upcoming();
            (up.first().cloned(), up.iter().skip(1).cloned().collect())
        } else {
            (self.queue.current().cloned(), self.queue.upcoming().to_vec())
        };
        // A jam track waiting for the shared start time shows as loading, not paused.
        let jam_hold = self.jam.as_ref().is_some_and(|j| j.hold) && !self.user_paused && !self.empty_paused();
        let status = match &self.playing {
            Some(_) if jam_hold => Status::Loading,
            Some(_) if self.paused() => Status::Paused,
            Some(_) => Status::Playing,
            None if self.loading.is_some() => Status::Loading,
            None if self.waiting_start => Status::Paused,
            None => Status::Idle,
        };
        let paused_reason = (status == Status::Paused).then(|| {
            if self.empty_paused() {
                PausedReason::EmptyChannel
            } else {
                PausedReason::User
            }
        });
        let rule = self.settings.vote_skip;
        PlayerSnapshot {
            version: super::state::next_version(),
            guild_id: self.guild_id,
            channel_id: self.voice_channel_id,
            text_channel_id: self.text_channel,
            bot_index: self.bot.index,
            bot_user_id: self.bot.user_id(),
            status,
            paused_reason,
            empty_channel_deadline: self.empty.deadline().map(deadline_epoch_ms),
            current: current.clone(),
            position_ms: self.shown_position_ms(),
            position_at: epoch_ms(),
            upcoming,
            history: self.queue.history(HISTORY_LEN),
            loop_mode: self.queue.loop_mode,
            volume: self.volume,
            vote_skip_enabled: rule.enabled,
            skip_votes: (rule.enabled && current.is_some()).then(|| {
                let listeners = self.listeners();
                (self.votes.current(&listeners), required_votes(&rule, listeners.len()))
            }),
            ai: self.radio.as_ref().map(Radio::snapshot),
            jam: self.jam.as_ref().map(|j| j.view.clone()),
        }
    }

    fn publish(&self) {
        let snap = Arc::new(self.snapshot());
        match self.slot.write() {
            Ok(mut s) => *s = snap.clone(),
            Err(p) => *p.into_inner() = snap.clone(),
        }
        let _ = self.events.send(PlayerEvent::State(snap));
    }

    /// Position shown to the panel: the playing position, or the position a loading track will
    /// start at (a seek asked while it loads, or a restored position) until that seek is applied.
    fn shown_position_ms(&self) -> u64 {
        let pending = self.resume_at.as_ref();
        match &self.playing {
            Some(p) => match pending {
                Some(r) if r.uid == p.entry.uid && !p.became_playable => r.target_ms,
                _ => p.position_ms(),
            },
            None => match (pending, self.loading) {
                (Some(r), Some(uid)) if r.uid == uid => r.target_ms,
                _ => 0,
            },
        }
    }

    /// Position saved with the session: the saved position of a restored track until its seek is
    /// applied, else the playing position.
    fn checkpoint_ms(&self) -> u64 {
        if let Some(saved) = self.jam.as_ref().and_then(|j| j.saved.as_ref()) {
            return saved.position_ms;
        }
        if let Some(r) = &self.resume_at {
            if self.queue.cursor().is_some_and(|e| e.uid == r.uid) {
                return r.saved_ms;
            }
        }
        self.playing.as_ref().map(Playing::position_ms).unwrap_or(0)
    }

    /// Queue the session write (debounced by the writer). `tracks`: the track list changed.
    fn persist(&mut self, tracks: bool) {
        let position = self.checkpoint_ms();
        self.persist_at(tracks, position);
    }

    fn persist_at(&mut self, tracks: bool, position_ms: u64) {
        // Any write carries the position, so the next periodic checkpoint is due from now.
        self.checkpoint_at = Instant::now() + CHECKPOINT_EVERY;
        let record = SessionRecord {
            guild_id: self.guild_id,
            voice_channel_id: self.voice_channel_id,
            bot_user_id: self.bot.user_id(),
            text_channel_id: self.text_channel.unwrap_or(0),
            current_index: self.queue.index() as u32,
            loop_mode: self.queue.loop_mode,
            volume: self.volume,
            paused: self.user_paused,
            started_by: self.started_by,
            ai_state: None,
            position_ms,
            updated_at: 0,
        };
        let mut record = record;
        // A jam member keeps its own queue saved (it resumes that one after a restart).
        let list = match self.jam.as_ref().and_then(|j| j.saved.as_ref()) {
            Some(saved) => {
                record.current_index = saved.index as u32;
                record.loop_mode = saved.loop_mode;
                record.paused = saved.paused;
                tracks.then(|| saved.tracks.iter().map(|e| e.to_session()).collect())
            }
            None => tracks.then(|| self.queue.all().iter().map(|e| e.to_session()).collect()),
        };
        self.deps.sessions.save(record, list);
    }

    async fn finish(&mut self, reason: EndReason) {
        info!(
            guild = self.guild_id,
            channel = self.voice_channel_id,
            ?reason,
            "player ending"
        );
        // Leave the jam; a member's own queue is what gets saved (restart) or kept ("Play it again").
        if let Some(link) = self.jam.take() {
            link.handle.post(JamCmd::Leave { key: self.key() });
            if let Some(saved) = link.saved {
                self.queue = Queue::restore(saved.tracks, saved.index, saved.loop_mode);
                self.user_paused = saved.paused;
                self.resume_at = self.queue.cursor().map(|e| PendingResume {
                    uid: e.uid,
                    saved_ms: saved.position_ms,
                    target_ms: saved.position_ms,
                });
            }
        }
        let restore = Restore {
            tracks: self.queue.all().to_vec(),
            index: self.queue.index().min(self.queue.all().len().saturating_sub(1)),
            loop_mode: self.queue.loop_mode,
            volume: self.volume,
            position_ms: 0,
            after_restart: false,
        };
        if let Some(r) = self.radio.as_mut() {
            r.stop();
        }
        let position = self.checkpoint_ms();
        if let Some(p) = self.playing.take() {
            let _ = p.handle.stop();
            if reason != EndReason::Shutdown {
                self.record_play(&p);
            }
        }
        // The channel dirs (heads included) are removed below.
        let _ = self.prefetch.clear();
        if let Some((_, task)) = self.prefetch_task.take() {
            task.abort();
        }

        if reason == EndReason::Shutdown {
            // Exact position at shutdown; main flushes the writer once every player has ended.
            self.persist_at(true, position);
        } else {
            self.deps.sessions.delete(self.key());
        }

        // Really disconnect (also cleans songbird's call after a kick).
        let guild = GuildId::new(self.guild_id);
        if tokio::time::timeout(VOICE_TIMEOUT, self.bot.songbird.remove(guild))
            .await
            .is_err()
        {
            warn!(guild = self.guild_id, "leaving the voice channel timed out");
        }
        if reason != EndReason::GuildGone {
            self.deps.bots.set_voice(self.guild_id, self.bot.index, None);
        }

        for c in self.old_channels.iter().copied().chain([self.voice_channel_id]) {
            self.deps.procs.kill_channel(self.guild_id, c).await;
            let dirs = self.deps.sources.stream_dirs();
            self.manager.heads().forget_dir(&dirs.channel_path(self.guild_id, c));
            dirs.remove_channel(self.guild_id, c).await;
        }

        let minutes = self.deps.config.empty_channel_timeout_secs.div_ceil(60).max(1);
        match reason {
            EndReason::EmptyTimeout => {
                self.messenger.notice(OwnedNotice::QueueDeleted { minutes });
                self.manager.remember_ended(self.key(), restore);
            }
            EndReason::TooManyFailures => self.messenger.notice(OwnedNotice::TooManyFailures),
            EndReason::Kicked => self.messenger.notice(OwnedNotice::Disconnected),
            EndReason::Stopped { by, announce: true } => self.messenger.notice(OwnedNotice::Stopped { by }),
            _ => {}
        }
        self.messenger.close(true).await;
        self.manager.remove(self.key(), self.generation);
        let _ = self.events.send(PlayerEvent::Closed);
    }
}

#[cfg(test)]
mod tests {
    use serenity::{all::UserId, cache::Cache, http::Http};
    use songbird::Songbird;
    use tokio::sync::oneshot;

    use super::*;
    use crate::{
        ai::AiEngine,
        discord::registry::BotRegistry,
        i18n::I18n,
        player::{PlayerDeps, QueueEntry, Requester},
        procs::ProcessRegistry,
        sources::Sources,
        store::{test_util::track, GuildSettings, LoopMode, SessionWriter, Store},
    };

    const GUILD: u64 = 11;
    const VOICE: u64 = 22;

    /// Restore a session into a real actor with nobody in the channel (so nothing is fetched), shut
    /// it down and return the saved (current_index, position_ms).
    async fn restore_then_shutdown(position_ms: u64, index: usize) -> (u32, u64) {
        let dir = std::env::temp_dir().join(format!("lynx-ckpt-test-{}", Uuid::new_v4()));
        let config = Arc::new(crate::ai::test_util::config(dir));
        let store = Store::open_in_memory().await.unwrap();
        let procs = ProcessRegistry::new();
        let sources = Sources::new(&config, store.clone(), procs.clone()).unwrap();
        let (sessions, _task) = SessionWriter::spawn_with_interval(store.clone(), Duration::from_secs(3600));
        let bot = Arc::new(BotInfo::new(
            0,
            Arc::new(Http::new("x")),
            Arc::new(Cache::new()),
            Songbird::serenity(),
        ));
        let bots = Arc::new(BotRegistry::new(vec![bot.clone()]));
        let ai = AiEngine::new(&config, store.clone(), sources.clone());
        let manager = PlayerManager::new(PlayerDeps {
            config: config.clone(),
            i18n: Arc::new(I18n::load("en").unwrap()),
            store: store.clone(),
            sources,
            procs,
            sessions: sessions.clone(),
            bots,
            ai,
            http: reqwest::Client::new(),
        });

        let tracks: Vec<Entry> = ["a", "b", "c"]
            .iter()
            .map(|id| Arc::new(QueueEntry::new(track(id), Requester::id_only(1))))
            .collect();
        let req = StartRequest {
            guild_id: GUILD,
            voice_channel_id: VOICE,
            bot_index: 0,
            text_channel_id: None,
            started_by: 1,
            humans: HashSet::new(),
            settings: GuildSettings::default(),
            restore: Some(Restore {
                tracks,
                index,
                loop_mode: LoopMode::Off,
                volume: 100,
                position_ms,
                after_restart: true,
            }),
        };
        let (tx, rx) = mpsc::channel(64);
        let (events, _) = broadcast::channel(16);
        let snapshot = Arc::new(RwLock::new(Arc::new(initial_snapshot(&req, 0))));
        let call = Arc::new(Mutex::new(Call::standalone(GuildId::new(GUILD), UserId::new(99))));
        let task = tokio::spawn(run(Spawn {
            manager,
            req,
            call,
            bot,
            generation: 1,
            tx: tx.clone(),
            rx,
            events,
            snapshot,
        }));
        tx.send(Command::End(EndReason::Shutdown)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .unwrap()
            .unwrap();
        sessions.flush_now().await.unwrap();
        let saved = store
            .get_session(SessionKey {
                guild_id: GUILD,
                voice_channel_id: VOICE,
            })
            .await
            .unwrap()
            .expect("shutdown keeps the session");
        assert_eq!(saved.tracks.len(), 3);
        (saved.session.current_index, saved.session.position_ms)
    }

    type Slot = Arc<RwLock<Arc<PlayerSnapshot>>>;

    async fn test_manager() -> (PlayerManager, Arc<BotInfo>) {
        let dir = std::env::temp_dir().join(format!("lynx-actor-test-{}", Uuid::new_v4()));
        let config = Arc::new(crate::ai::test_util::config(dir));
        let store = Store::open_in_memory().await.unwrap();
        let procs = ProcessRegistry::new();
        let sources = Sources::new(&config, store.clone(), procs.clone()).unwrap();
        let (sessions, _task) = SessionWriter::spawn_with_interval(store.clone(), Duration::from_secs(3600));
        let bot = Arc::new(BotInfo::new(
            0,
            Arc::new(Http::new("x")),
            Arc::new(Cache::new()),
            Songbird::serenity(),
        ));
        let bots = Arc::new(BotRegistry::new(vec![bot.clone()]));
        let ai = AiEngine::new(&config, store.clone(), sources.clone());
        let manager = PlayerManager::new(PlayerDeps {
            config: config.clone(),
            i18n: Arc::new(I18n::load("en").unwrap()),
            store: store.clone(),
            sources,
            procs,
            sessions,
            bots,
            ai,
            http: reqwest::Client::new(),
        });
        (manager, bot)
    }

    /// A real actor with nobody in the channel (so nothing is fetched), optionally with a restored
    /// queue that waits for people.
    fn spawn_on(
        manager: &PlayerManager,
        bot: &Arc<BotInfo>,
        voice: u64,
        restore: Option<Restore>,
    ) -> (mpsc::Sender<Command>, Slot) {
        let req = StartRequest {
            guild_id: GUILD,
            voice_channel_id: voice,
            bot_index: 0,
            text_channel_id: None,
            started_by: 1,
            humans: HashSet::new(),
            settings: GuildSettings::default(),
            restore,
        };
        let (tx, rx) = mpsc::channel(64);
        let (events, _) = broadcast::channel(16);
        let snapshot = Arc::new(RwLock::new(Arc::new(initial_snapshot(&req, 0))));
        let call = Arc::new(Mutex::new(Call::standalone(GuildId::new(GUILD), UserId::new(99))));
        tokio::spawn(run(Spawn {
            manager: manager.clone(),
            req,
            call,
            bot: bot.clone(),
            generation: voice,
            tx: tx.clone(),
            rx,
            events,
            snapshot: snapshot.clone(),
        }));
        (tx, snapshot)
    }

    /// A real actor with nobody in the channel and an empty queue (nothing is fetched).
    async fn idle_actor() -> (mpsc::Sender<Command>, Slot) {
        let (manager, bot) = test_manager().await;
        spawn_on(&manager, &bot, VOICE, None)
    }

    fn restore_of(ids: &[&str]) -> Restore {
        Restore {
            tracks: ids
                .iter()
                .map(|id| Arc::new(QueueEntry::new(track(id), Requester::id_only(1))))
                .collect(),
            index: 0,
            loop_mode: LoopMode::Off,
            volume: 100,
            position_ms: 0,
            after_restart: false,
        }
    }

    async fn ask<T>(tx: &mpsc::Sender<Command>, make: impl FnOnce(oneshot::Sender<T>) -> Command) -> T {
        let (r, rx) = oneshot::channel();
        tx.send(make(r)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), rx).await.unwrap().unwrap()
    }

    /// Wait until the snapshot satisfies `f`.
    async fn until_snap(slot: &Slot, f: impl Fn(&PlayerSnapshot) -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !f(&slot.read().unwrap()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("snapshot condition");
    }

    fn uids(s: &PlayerSnapshot) -> Vec<String> {
        s.current
            .iter()
            .chain(s.upcoming.iter())
            .map(|e| e.info.youtube_id.clone().unwrap_or_default())
            .collect()
    }

    #[tokio::test]
    async fn jam_members_share_the_queue_and_get_their_own_back() {
        let (manager, bot) = test_manager().await;
        // Host and member wait for people (nothing plays, nothing is fetched).
        let (host, host_slot) = spawn_on(&manager, &bot, VOICE, Some(restore_of(&["a", "b"])));
        let (member, member_slot) = spawn_on(&manager, &bot, VOICE + 1, Some(restore_of(&["x", "y", "z"])));
        until_snap(&member_slot, |s| uids(s) == ["x", "y", "z"]).await;

        let jam = ask(&host, |reply| Command::JamCreate { reply }).await.unwrap();
        assert!(
            ask(&host, |reply| Command::JamCreate { reply }).await.is_err(),
            "already hosting"
        );
        ask(&member, |reply| Command::JamJoin {
            jam: jam.clone(),
            reply,
        })
        .await
        .unwrap();
        until_snap(&member_slot, |s| s.jam.as_ref().is_some_and(|j| j.members.len() == 2)).await;
        // The member shows the shared queue (the host's).
        until_snap(&member_slot, |s| uids(s) == ["a", "b"]).await;

        // A shared edit from the member reaches the host.
        ask(&member, |reply| Command::Loop {
            mode: LoopMode::Queue,
            reply,
        })
        .await;
        until_snap(&host_slot, |s| s.loop_mode == LoopMode::Queue).await;
        until_snap(&member_slot, |s| s.loop_mode == LoopMode::Queue).await;
        // Volume stays per channel.
        ask(&member, |reply| Command::Volume { volume: 40, reply }).await;
        assert_eq!(host_slot.read().unwrap().volume, 100);

        // Host-only editing: the member's edit changes nothing.
        jam.settings(jam::SettingsPatch {
            editable_by: Some(jam::EditRule::Host),
            ..Default::default()
        })
        .await
        .unwrap();
        let n = ask(&member, |reply| Command::Clear { reply }).await;
        assert_eq!(n, 0);
        assert_eq!(uids(&host_slot.read().unwrap()), ["a", "b"]);

        // Leaving brings the member's own queue back.
        assert!(ask(&member, |reply| Command::JamLeave { reply }).await);
        until_snap(&member_slot, |s| s.jam.is_none() && uids(s) == ["x", "y", "z"]).await;
        until_snap(&host_slot, |s| s.jam.as_ref().is_some_and(|j| j.members.len() == 1)).await;
        assert!(manager
            .jams()
            .of(SessionKey {
                guild_id: GUILD,
                voice_channel_id: VOICE + 1
            })
            .is_none());

        // Join again; then the host's player ends: nobody else has people, so the jam ends and the
        // member's own queue comes back.
        ask(&member, |reply| Command::JamJoin {
            jam: jam.clone(),
            reply,
        })
        .await
        .unwrap();
        until_snap(&member_slot, |s| uids(s) == ["a", "b"]).await;
        host.send(Command::End(EndReason::EmptyTimeout)).await.unwrap();
        until_snap(&member_slot, |s| s.jam.is_none() && uids(s) == ["x", "y", "z"]).await;
        assert_eq!(manager.jams().len(), 0);
        member.send(Command::End(EndReason::Shutdown)).await.unwrap();
    }

    /// Host and member with empty queues, both with someone in the channel, in one jam.
    async fn jam_pair(
        manager: &PlayerManager,
        bot: &Arc<BotInfo>,
    ) -> ((mpsc::Sender<Command>, Slot), (mpsc::Sender<Command>, Slot), JamHandle) {
        let (host, host_slot) = spawn_on(manager, bot, VOICE, None);
        let (member, member_slot) = spawn_on(manager, bot, VOICE + 1, None);
        for tx in [&host, &member] {
            tx.send(Command::Humans(HashSet::from([5]))).await.unwrap();
        }
        until_snap(&host_slot, |s| s.empty_channel_deadline.is_none()).await;
        until_snap(&member_slot, |s| s.empty_channel_deadline.is_none()).await;
        let jam = ask(&host, |reply| Command::JamCreate { reply }).await.unwrap();
        ask(&member, |reply| Command::JamJoin {
            jam: jam.clone(),
            reply,
        })
        .await
        .unwrap();
        until_snap(&host_slot, |s| s.jam.as_ref().is_some_and(|j| j.members.len() == 2)).await;
        ((host, host_slot), (member, member_slot), jam)
    }

    async fn until_closed(tx: &mpsc::Sender<Command>) {
        tokio::time::timeout(Duration::from_secs(5), tx.closed())
            .await
            .expect("player ended");
    }

    /// Everyone leaves a member channel: its own timer starts (the host's does not), and at the
    /// deadline (1 s in tests) it leaves the jam and its player ends.
    #[tokio::test]
    async fn empty_jam_member_leaves_the_jam_at_the_deadline() {
        let (manager, bot) = test_manager().await;
        let ((host, host_slot), (member, member_slot), jam) = jam_pair(&manager, &bot).await;
        member.send(Command::Humans(HashSet::new())).await.unwrap();
        until_snap(&member_slot, |s| s.empty_channel_deadline.is_some()).await;
        assert!(host_slot.read().unwrap().empty_channel_deadline.is_none());
        until_snap(&host_slot, |s| s.jam.as_ref().is_some_and(|j| j.members.len() == 1)).await;
        until_closed(&member).await;
        assert!(!jam.view().ended);
        assert!(host_slot.read().unwrap().empty_channel_deadline.is_none());
        host.send(Command::End(EndReason::Shutdown)).await.unwrap();
    }

    /// Someone comes back before the deadline: the timer is cancelled and the channel stays.
    #[tokio::test]
    async fn jam_member_stays_when_someone_returns() {
        let (manager, bot) = test_manager().await;
        let ((host, host_slot), (member, member_slot), _jam) = jam_pair(&manager, &bot).await;
        member.send(Command::Humans(HashSet::new())).await.unwrap();
        until_snap(&member_slot, |s| s.empty_channel_deadline.is_some()).await;
        member.send(Command::Humans(HashSet::from([6]))).await.unwrap();
        until_snap(&member_slot, |s| s.empty_channel_deadline.is_none()).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!member.is_closed());
        assert_eq!(host_slot.read().unwrap().jam.as_ref().map(|j| j.members.len()), Some(2));
        for tx in [&member, &host] {
            tx.send(Command::End(EndReason::Shutdown)).await.unwrap();
        }
    }

    /// The host channel empties: at the deadline the host role passes to the member with people.
    #[tokio::test]
    async fn empty_jam_host_passes_the_host_role() {
        let (manager, bot) = test_manager().await;
        let ((host, _host_slot), (member, member_slot), jam) = jam_pair(&manager, &bot).await;
        let member_key = SessionKey {
            guild_id: GUILD,
            voice_channel_id: VOICE + 1,
        };
        host.send(Command::Humans(HashSet::new())).await.unwrap();
        until_snap(&member_slot, |s| {
            s.jam
                .as_ref()
                .is_some_and(|j| j.members.len() == 1 && j.host == member_key)
        })
        .await;
        until_closed(&host).await;
        assert!(!jam.view().ended);
        assert!(member_slot.read().unwrap().empty_channel_deadline.is_none());
        member.send(Command::End(EndReason::Shutdown)).await.unwrap();
    }

    fn version(slot: &Arc<RwLock<Arc<PlayerSnapshot>>>) -> u64 {
        slot.read().unwrap().version
    }

    /// Each failed seek of a track moves one step on: fresh input, fresh URL, then no more seeks.
    /// Replaying the steps as the actor does always ends within three failures.
    #[test]
    fn failed_seeks_recover_in_bounded_steps() {
        let mut reresolved = false;
        let mut steps = Vec::new();
        for failures in 1..=5 {
            let step = seek_recovery(failures, reresolved);
            if step == SeekRecovery::Reresolve {
                reresolved = true;
            }
            steps.push(step);
            if step == SeekRecovery::GiveUp {
                break;
            }
        }
        assert_eq!(
            steps,
            [SeekRecovery::Restart, SeekRecovery::Reresolve, SeekRecovery::GiveUp]
        );
        // Already re-resolved (the URL was refused before playing): no second resolve.
        assert_eq!(seek_recovery(2, true), SeekRecovery::GiveUp);
        assert_eq!(seek_recovery(9, false), SeekRecovery::GiveUp);
    }

    /// A seek songbird dropped because the track was busy is retried later, not treated as a
    /// broken track.
    #[test]
    fn replaced_seek_is_not_a_failure() {
        assert!(seek_was_replaced(&ControlError::Dropped));
        assert!(!seek_was_replaced(&ControlError::Finished));
    }

    /// The jam never seeks a track songbird cannot seek yet (the seek would be dropped and count
    /// as a failure); the line-up runs when it becomes playable. Once seeking was given up the
    /// track plays unsynced.
    #[test]
    fn jam_seeks_only_playable_tracks() {
        assert!(!jam_may_seek(false, false), "still loading: keep the target");
        assert!(jam_may_seek(true, false));
        assert!(!jam_may_seek(true, true), "seeking given up: play unsynced");
    }

    #[test]
    fn seek_target_keeps_the_last_second_and_refuses_past_the_end() {
        assert_eq!(seek_target(60_000, Some(180_000)), Ok(60_000));
        assert_eq!(seek_target(180_000, Some(180_000)), Ok(179_000));
        assert_eq!(seek_target(180_001, Some(180_000)), Err(SeekOutcome::OutOfRange));
        assert_eq!(seek_target(500_000, None), Ok(500_000));
        assert_eq!(seek_target(500_000, Some(0)), Ok(500_000));
    }

    #[tokio::test]
    async fn seek_with_nothing_playing_or_loading_is_refused() {
        let (tx, _slot) = idle_actor().await;
        let outcome = ask(&tx, |reply| Command::Seek {
            position_ms: 1_000,
            reply,
        })
        .await;
        assert_eq!(outcome, SeekOutcome::NothingPlaying);
        tx.send(Command::End(EndReason::Shutdown)).await.unwrap();
    }

    #[tokio::test]
    async fn seek_while_the_track_loads_is_kept_for_when_it_plays() {
        let (manager, bot) = test_manager().await;
        let req = StartRequest {
            guild_id: GUILD,
            voice_channel_id: VOICE,
            bot_index: 0,
            text_channel_id: None,
            started_by: 1,
            humans: HashSet::from([1]),
            settings: GuildSettings::default(),
            restore: Some(restore_of(&["a", "b"])),
        };
        let (tx, rx) = mpsc::channel(64);
        let (events, mut ev) = broadcast::channel(64);
        let snapshot = Arc::new(RwLock::new(Arc::new(initial_snapshot(&req, 0))));
        let call = Arc::new(Mutex::new(Call::standalone(GuildId::new(GUILD), UserId::new(99))));
        // Queued before the actor boots, so it lands while the first track is being prepared
        // (the prepare result comes back on the same channel, after it).
        let (r, seek_rx) = oneshot::channel();
        tx.send(Command::Seek {
            position_ms: 60_000,
            reply: r,
        })
        .await
        .unwrap();
        tokio::spawn(run(Spawn {
            manager,
            req,
            call,
            bot,
            generation: 1,
            tx: tx.clone(),
            rx,
            events,
            snapshot,
        }));
        let outcome = tokio::time::timeout(Duration::from_secs(5), seek_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome, SeekOutcome::Done);
        let mut shown = false;
        let mut position = false;
        while let Ok(e) = ev.try_recv() {
            match e {
                PlayerEvent::State(s) if s.status == Status::Loading && s.position_ms == 60_000 => shown = true,
                PlayerEvent::Position {
                    position_ms: 60_000, ..
                } => position = true,
                _ => {}
            }
        }
        assert!(shown, "the loading snapshot shows the requested position");
        assert!(position, "the position event carries the requested position");
        tx.send(Command::End(EndReason::Shutdown)).await.unwrap();
    }

    #[tokio::test]
    async fn every_change_publishes_a_newer_snapshot() {
        let (tx, slot) = idle_actor().await;
        let mut last = version(&slot);
        let mut check = |what: &str| {
            let v = version(&slot);
            assert!(v > last, "{what}: {v} <= {last}");
            last = v;
        };
        let (r, rx) = oneshot::channel();
        tx.send(Command::Volume { volume: 50, reply: r }).await.unwrap();
        rx.await.unwrap();
        check("volume");
        let (r, rx) = oneshot::channel();
        tx.send(Command::Loop {
            mode: LoopMode::Queue,
            reply: r,
        })
        .await
        .unwrap();
        rx.await.unwrap();
        check("loop");
        let (r, rx) = oneshot::channel();
        tx.send(Command::Shuffle { reply: r }).await.unwrap();
        rx.await.unwrap();
        check("shuffle");
        let (r, rx) = oneshot::channel();
        tx.send(Command::Clear { reply: r }).await.unwrap();
        rx.await.unwrap();
        check("clear");
        assert_eq!(slot.read().unwrap().volume, 50);
        tx.send(Command::End(EndReason::Shutdown)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn track_clock_across_pause_resume_and_seek() {
        let mut c = TrackClock::new(true);
        tokio::time::advance(Duration::from_millis(10_000)).await;
        assert_eq!(c.position_ms(), 10_000);
        c.stop();
        tokio::time::advance(Duration::from_millis(5_000)).await;
        assert_eq!(c.position_ms(), 10_000, "paused time does not count");
        assert!(!c.running());
        c.set_position(60_000);
        assert!(!c.running(), "a seek keeps the pause");
        assert_eq!(c.position_ms(), 60_000);
        c.start();
        tokio::time::advance(Duration::from_millis(2_000)).await;
        assert_eq!(c.position_ms(), 62_000);
        c.set_position(1_000);
        assert!(c.running(), "a seek keeps it running");
        tokio::time::advance(Duration::from_millis(500)).await;
        assert_eq!(c.position_ms(), 1_500);
        assert_eq!(c.listened(), Duration::from_millis(12_500));
        // A track created paused stays at 0.
        let p = TrackClock::new(false);
        tokio::time::advance(Duration::from_millis(3_000)).await;
        assert_eq!(p.position_ms(), 0);
    }

    #[tokio::test]
    async fn restored_checkpoint_survives_a_restart_before_it_plays() {
        // The track never started (nobody in the channel): the saved position is kept, not reset.
        assert_eq!(restore_then_shutdown(90_000, 1).await, (1, 90_000));
    }

    #[tokio::test]
    async fn restored_track_near_its_end_moves_to_the_next_one() {
        // 180 s tracks: 177 s is within the last 5 s.
        assert_eq!(restore_then_shutdown(177_000, 0).await, (1, 0));
        // No checkpoint: same track from the start.
        assert_eq!(restore_then_shutdown(0, 2).await, (2, 0));
    }

    #[tokio::test]
    async fn late_music_metadata_updates_the_queue_once() {
        let (manager, bot) = test_manager().await;
        let restore = restore_of(&["a", "b"]);
        let (first, second) = (restore.tracks[0].uid, restore.tracks[1].uid);
        let (tx, slot) = spawn_on(&manager, &bot, VOICE, Some(restore));
        let send = |uid: Uuid, title: &str| {
            let music = MusicMeta {
                youtube_id: "song0000000".into(),
                title: title.into(),
                artists: vec!["Artist".into()],
                album: None,
                isrc: None,
            };
            tx.send(Command::MusicMeta { uid, music })
        };
        let title = |e: Option<&Entry>| e.map(|e| e.info.display_title().to_owned());
        send(first, "Clean Title").await.unwrap();
        until_snap(&slot, |s| title(s.current.as_ref()).as_deref() == Some("Clean Title")).await;
        // An upcoming entry is updated too.
        send(second, "Second").await.unwrap();
        until_snap(&slot, |s| title(s.upcoming.first()).as_deref() == Some("Second")).await;
        // Metadata already known is not replaced by a late answer; unknown entries are ignored.
        send(first, "Other").await.unwrap();
        send(Uuid::new_v4(), "Nobody").await.unwrap();
        // Commands run in order: once this answers, the ones above were handled.
        let _ = ask(&tx, |reply| Command::Pause { reply }).await;
        let snap = slot.read().unwrap().clone();
        assert_eq!(title(snap.current.as_ref()).as_deref(), Some("Clean Title"));
        // The playback id never changes.
        assert_eq!(snap.current.as_ref().unwrap().info.youtube_id.as_deref(), Some("a"));
        tx.send(Command::End(EndReason::Shutdown)).await.unwrap();
    }
}

/// A skip meant for `expected` while `current` plays: true when it is about another track (the
/// first of a double click already skipped it). No expectation, or nothing current, is not stale.
fn stale(expected: Option<Uuid>, current: Option<Uuid>) -> bool {
    matches!((expected, current), (Some(e), Some(c)) if e != c)
}

#[cfg(test)]
mod stale_tests {
    use super::*;

    #[test]
    fn a_skip_for_another_track_is_stale() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(stale(Some(a), Some(b)));
        assert!(!stale(Some(a), Some(a)));
        assert!(!stale(None, Some(a)));
        assert!(!stale(Some(a), None));
    }
}
