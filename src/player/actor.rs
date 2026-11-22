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
use songbird::{tracks::TrackHandle, Call, Event};
use tokio::{
    sync::{broadcast, mpsc, Mutex},
    time::{sleep_until, Instant},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::{
    lifecycle::{deadline_epoch_ms, EmptyAction, EmptyChannel, QUEUE_END_LEAVE_AFTER},
    messages::{Messenger, MessengerDeps, NpData, OwnedNotice},
    playback::{http_input, track_events, ytdlp_input, TrackNotifier, TrackSignal},
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
    sources::{Playable, SourceError},
    store::{track_key, GuildSettings, SessionKey, SessionRecord, MAX_VOLUME},
};

/// Prefetch the next track this long before the current one ends.
/// The next track is prepared shortly after the current one starts (stream URLs last hours), so
/// skips and track changes start at once instead of waiting for yt-dlp and YouTube's `available_at`.
const PREFETCH_AFTER_START: Duration = Duration::from_secs(3);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_FAILURES: u32 = 3;
const POSITION_TICK: Duration = Duration::from_secs(10);
const VOICE_TIMEOUT: Duration = Duration::from_secs(5);
const SEEK_TIMEOUT: Duration = Duration::from_secs(10);
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
    }
}

/// The track in songbird and its clock.
struct Playing {
    entry: Entry,
    /// What the input was built from (a failed seek plays it again from the target).
    playable: Playable,
    handle: TrackHandle,
    /// Position when `since` started.
    base_ms: u64,
    /// Running since (None while paused).
    since: Option<Instant>,
    /// Listening time before `since`.
    listened: Duration,
}

impl Playing {
    fn position_ms(&self) -> u64 {
        self.base_ms + self.since.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0)
    }

    fn listened(&self) -> Duration {
        self.listened + self.since.map(|s| s.elapsed()).unwrap_or_default()
    }

    fn stop_clock(&mut self) {
        if let Some(s) = self.since.take() {
            self.base_ms += s.elapsed().as_millis() as u64;
            self.listened += s.elapsed();
        }
    }

    fn start_clock(&mut self) {
        if self.since.is_none() {
            self.since = Some(Instant::now());
        }
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
    prefetched: Option<(Uuid, Playable)>,
    prefetching: Option<Uuid>,
    prefetch_at: Option<Instant>,
    idle_leave_at: Option<Instant>,
    next_tick: Instant,

    user_paused: bool,
    empty: EmptyChannel,
    /// Restored queue not started yet (nobody was in the channel).
    waiting_start: bool,
    /// Post "Back online" when the restored track starts.
    back_online: bool,
    humans: HashSet<u64>,
    votes: Votes,
    failures: u32,
    rng: fastrand::Rng,
    ending: Option<EndReason>,
    radio: Option<Radio>,
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
        volume: s.req.settings.default_volume.min(MAX_VOLUME),
        settings: s.req.settings,
        started_by: s.req.started_by,
        text_channel: s.req.text_channel_id,
        messenger,
        playing: None,
        loading: None,
        prefetched: None,
        prefetching: None,
        prefetch_at: None,
        idle_leave_at: None,
        next_tick: Instant::now() + POSITION_TICK,
        user_paused: false,
        empty: EmptyChannel::default(),
        waiting_start: false,
        back_online: false,
        humans: s.req.humans,
        votes: Votes::default(),
        failures: 0,
        rng: fastrand::Rng::new(),
        ending: None,
        radio: None,
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
        self.user_paused || self.empty.is_waiting()
    }

    async fn boot(&mut self, restore: Option<Restore>) {
        if let Some(r) = restore {
            self.queue = super::queue::Queue::restore(r.tracks, r.index, r.loop_mode);
            self.volume = r.volume.min(MAX_VOLUME);
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
            let tick = (self.events.receiver_count() > 0 && self.playing.as_ref().is_some_and(|p| p.since.is_some()))
                .then_some(self.next_tick);
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
                _ = until(self.prefetch_at) => self.start_prefetch(),
                _ = until(tick) => {
                    self.next_tick = Instant::now() + POSITION_TICK;
                    if let Some(p) = &self.playing {
                        let _ = self.events.send(PlayerEvent::Position { position_ms: p.position_ms(), position_at: epoch_ms() });
                    }
                }
            }
        }
    }

    async fn handle(&mut self, cmd: Command) {
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
                reply,
            } => {
                let outcome = self.skip(user, force, &rule).await;
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
                let outcome = self.seek(position_ms).await;
                let _ = reply.send(outcome);
            }
            Command::Volume { volume, reply } => {
                let prev = self.volume;
                self.volume = volume.min(MAX_VOLUME);
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
                self.refresh_prefetch();
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
                self.volume = self.volume.min(MAX_VOLUME);
                self.settings = s;
                self.publish();
            }
            Command::SetTextChannel(c) => self.set_text_channel(c),
            Command::Humans(h) => {
                self.humans = h;
                self.on_humans_changed().await;
            }
            Command::Moved(to) => self.moved(to).await,
            Command::End(reason) => self.ending = Some(reason),
            Command::Prepared { uid, prefetch, result } => self.on_prepared(uid, prefetch, *result).await,
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
            Command::RadioDisliked { user, uid } => {
                let current = self.queue.current().map(|e| e.uid);
                if self
                    .radio
                    .as_ref()
                    .is_some_and(|r| r.owner_dislike_skips(user, true, uid, current))
                {
                    self.next_track(true).await;
                    self.publish();
                }
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
                self.refresh_prefetch();
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
            Err(PushError::Full { .. }) => (0, true),
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
        self.refresh_prefetch();
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
        if let Some((uid, playable)) = self.prefetched.take() {
            if uid == entry.uid && playable.stream.expires_at > crate::store::now_secs() + 60 {
                self.begin(entry, playable).await;
                return;
            }
            if uid != entry.uid {
                self.cleanup_track(uid);
            }
        }
        self.loading = Some(entry.uid);
        self.spawn_prepare(&entry, false);
        self.publish();
    }

    fn spawn_prepare(&self, entry: &Entry, prefetch: bool) {
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
                prefetch,
                result: Box::new(result),
            };
            let _ = tokio::time::timeout(Duration::from_secs(10), tx.send(cmd)).await;
        });
    }

    async fn on_prepared(&mut self, uid: Uuid, prefetch: bool, result: Result<Playable, SourceError>) {
        if prefetch {
            if self.prefetching == Some(uid) {
                self.prefetching = None;
                match result {
                    Ok(p) => self.prefetched = Some((uid, p)),
                    Err(e) => debug!(%uid, error = %e, "prefetch failed; will retry when the track starts"),
                }
            }
            return;
        }
        if self.loading != Some(uid) {
            return;
        }
        self.loading = None;
        let Some(entry) = self.queue.current().cloned().filter(|e| e.uid == uid) else {
            return;
        };
        match result {
            Ok(playable) => {
                // Lazy Spotify match: keep the matched video in the queue (and the saved session).
                // Also keep the YouTube Music metadata found while preparing (playlist entries).
                let entry = if playable.track.youtube_id != entry.info.youtube_id
                    || playable.track.music != entry.info.music
                {
                    let updated = Arc::new(QueueEntry {
                        info: playable.track.clone(),
                        ..(*entry).clone()
                    });
                    self.queue.update(updated.clone());
                    self.persist(true);
                    updated
                } else {
                    entry
                };
                self.begin(entry, playable).await;
            }
            Err(e) => self.failed(entry, &e.to_string()).await,
        }
    }

    async fn begin(&mut self, entry: Entry, playable: Playable) {
        let handle = match self.start_input(&entry, &playable).await {
            Ok(h) => h,
            Err(reason) => return self.failed(entry, &reason).await,
        };
        let paused = self.paused();
        self.playing = Some(Playing {
            entry: entry.clone(),
            playable,
            handle,
            base_ms: 0,
            since: (!paused).then(Instant::now),
            listened: Duration::ZERO,
        });
        self.schedule_prefetch();
        self.persist(false);
        self.publish();
        self.messenger.now_playing(self.np_data(&entry));
        let (ai, info) = (self.deps.ai.clone(), entry.info.clone());
        tokio::spawn(async move { ai.enqueue_tagging(&info).await });
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
    /// state attached.
    async fn start_input(&mut self, entry: &Entry, playable: &Playable) -> Result<TrackHandle, String> {
        let input = if playable.ytdlp_fallback {
            ytdlp_input(
                &self.deps.sources,
                &self.deps.procs,
                playable,
                &self.owner(Some(entry.uid)),
            )
            .await
            .map_err(|e| e.to_string())?
        } else {
            http_input(self.deps.http.clone(), &playable.stream)
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
        Ok(handle)
    }

    fn np_data(&self, entry: &Entry) -> NpData {
        NpData {
            entry: entry.clone(),
            node_name: (!self.bot.is_main()).then(|| self.bot.name().to_owned()),
            paused: self.paused(),
        }
    }

    /// Stop the songbird track (if any), count the play and free its resources.
    fn end_track(&mut self) {
        self.loading = None;
        self.prefetch_at = None;
        let Some(p) = self.playing.take() else { return };
        let _ = p.handle.stop();
        self.record_play(&p);
        let next_is_same = self.queue.current().is_some_and(|e| e.uid == p.entry.uid)
            && self.queue.loop_mode == crate::store::LoopMode::Track;
        if !next_is_same {
            self.cleanup_track(p.entry.uid);
        }
    }

    /// The current track finished (`skip: false`) or was skipped: move on.
    async fn next_track(&mut self, skip: bool) {
        self.end_track();
        match self.queue.advance(skip) {
            Some(_) => self.play_current().await,
            None => self.on_queue_end(),
        }
        self.persist(false);
    }

    async fn failed(&mut self, entry: Entry, reason: &str) {
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
            TrackSignal::Playable => self.failures = 0,
            TrackSignal::End => self.next_track(false).await,
            TrackSignal::Error => {
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
        self.prefetch_at = None;
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

    /// Kill the processes and remove the temp dir of one track, in the background.
    fn cleanup_track(&self, uid: Uuid) {
        let procs = self.deps.procs.clone();
        let dirs = self.deps.sources.stream_dirs().clone();
        let (g, c) = (self.guild_id, self.voice_channel_id);
        tokio::spawn(async move {
            procs.kill_track(g, c, uid).await;
            dirs.remove_track(g, c, uid).await;
        });
    }

    /// Pause or resume songbird to match `paused()`.
    fn apply_pause(&mut self) {
        let want = self.paused();
        if let Some(p) = &mut self.playing {
            if want && p.since.is_some() {
                let _ = p.handle.pause();
                p.stop_clock();
            } else if !want && p.since.is_none() {
                let _ = p.handle.play();
                p.start_clock();
            }
        }
        self.schedule_prefetch();
        self.persist(false);
        self.publish();
        if let Some(p) = &self.playing {
            self.messenger.refresh_now_playing(self.np_data(&p.entry));
        }
    }

    async fn seek(&mut self, position_ms: u64) -> SeekOutcome {
        let Some(p) = &mut self.playing else {
            return SeekOutcome::NothingPlaying;
        };
        let duration = p.entry.info.duration_ms.filter(|d| *d > 0);
        if duration.is_some_and(|d| position_ms > d) {
            return SeekOutcome::OutOfRange;
        }
        let target = duration
            .map(|d| position_ms.min(d.saturating_sub(1000)))
            .unwrap_or(position_ms);
        let seeked = match tokio::time::timeout(SEEK_TIMEOUT, p.handle.seek_async(Duration::from_millis(target))).await
        {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                warn!(guild = self.guild_id, track = %p.entry.info.url, target, error = %e, "seek failed; restarting the track there");
                false
            }
            Err(_) => {
                warn!(guild = self.guild_id, track = %p.entry.info.url, target, "seek timed out; restarting the track there");
                false
            }
        };
        if !seeked && !self.restart_at(target).await {
            return SeekOutcome::Failed;
        }
        let Some(p) = &mut self.playing else {
            return SeekOutcome::Failed;
        };
        let running = p.since.is_some();
        p.stop_clock();
        p.base_ms = target;
        if running {
            p.start_clock();
        }
        let pos = p.position_ms();
        self.schedule_prefetch();
        self.publish();
        let _ = self.events.send(PlayerEvent::Position {
            position_ms: pos,
            position_at: epoch_ms(),
        });
        SeekOutcome::Done
    }

    /// songbird ends a track whose seek failed: play the same stream again and seek the fresh input
    /// to `target`, so the track goes on instead of being skipped. Events of the old handle are
    /// ignored from here on (they no longer match the current handle).
    async fn restart_at(&mut self, target: u64) -> bool {
        let Some(p) = &self.playing else { return false };
        let (entry, playable) = (p.entry.clone(), p.playable.clone());
        let _ = p.handle.stop();
        let handle = match self.start_input(&entry, &playable).await {
            Ok(h) => h,
            Err(reason) => {
                self.failed(entry, &reason).await;
                return false;
            }
        };
        if let Some(p) = &mut self.playing {
            p.handle = handle.clone();
        }
        match tokio::time::timeout(SEEK_TIMEOUT, handle.seek_async(Duration::from_millis(target))).await {
            Ok(Ok(_)) => {
                info!(guild = self.guild_id, track = %entry.info.url, target, "track restarted at the seek target");
                true
            }
            // If the fresh input is really broken, its own error event skips the track.
            Ok(Err(e)) => {
                warn!(guild = self.guild_id, track = %entry.info.url, target, error = %e, "seek failed after restarting the track");
                false
            }
            Err(_) => {
                warn!(guild = self.guild_id, track = %entry.info.url, target, "seek timed out after restarting the track");
                false
            }
        }
    }

    async fn skip(&mut self, user: u64, force: bool, rule: &crate::store::VoteSkip) -> SkipOutcome {
        let Some(current) = self.queue.current().cloned() else {
            return SkipOutcome::NothingPlaying;
        };
        if !force && rule.enabled {
            let listeners = self.listeners();
            match self.votes.vote(user, &listeners, rule) {
                VoteResult::Passed => {}
                VoteResult::Counted { current, required } => {
                    self.publish();
                    return SkipOutcome::Voted { current, required };
                }
                VoteResult::AlreadyVoted { current, required } => {
                    return SkipOutcome::AlreadyVoted { current, required };
                }
            }
        }
        self.next_track(true).await;
        SkipOutcome::Skipped {
            skipped: current,
            next: self.queue.current().cloned(),
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
                info!(
                    guild = self.guild_id,
                    channel = self.voice_channel_id,
                    "channel empty, pausing"
                );
                self.apply_pause();
                self.messenger.notice(OwnedNotice::PausedEmpty {
                    deadline_secs: deadline_epoch_ms(deadline) / 1000,
                });
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
        self.publish();
    }

    async fn moved(&mut self, to: u64) {
        if to == self.voice_channel_id {
            return;
        }
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

    // ---- prefetch --------------------------------------------------------------------------

    fn schedule_prefetch(&mut self) {
        self.prefetch_at = self.playing.as_ref().and_then(|p| {
            p.since?;
            Some(Instant::now() + PREFETCH_AFTER_START)
        });
    }

    fn start_prefetch(&mut self) {
        self.prefetch_at = None;
        let Some(next) = self.queue.peek_next().cloned() else {
            return;
        };
        if self.queue.current().is_some_and(|c| c.uid == next.uid)
            || self.prefetching == Some(next.uid)
            || self.prefetched.as_ref().is_some_and(|(u, _)| *u == next.uid)
        {
            return;
        }
        self.prefetching = Some(next.uid);
        self.spawn_prepare(&next, true);
    }

    /// The next track may have changed: drop a prefetch that no longer matches.
    fn refresh_prefetch(&mut self) {
        let next = self.queue.peek_next().map(|e| e.uid);
        if let Some((uid, _)) = &self.prefetched {
            if Some(*uid) != next {
                let uid = *uid;
                self.prefetched = None;
                self.cleanup_track(uid);
            }
        }
        if self.prefetching.is_some() && self.prefetching != next {
            self.prefetching = None;
        }
        // Queue edits (add, move, remove, skip) can change the next track: prepare it soon.
        if next.is_some() && self.prefetched.is_none() && self.prefetching.is_none() && self.prefetch_at.is_none() {
            self.schedule_prefetch();
        }
        self.schedule_prefetch();
    }

    // ---- state -----------------------------------------------------------------------------

    fn snapshot(&self) -> PlayerSnapshot {
        let (current, upcoming) = if self.waiting_start {
            let up = self.queue.upcoming();
            (up.first().cloned(), up.iter().skip(1).cloned().collect())
        } else {
            (self.queue.current().cloned(), self.queue.upcoming().to_vec())
        };
        let status = match &self.playing {
            Some(_) if self.paused() => Status::Paused,
            Some(_) => Status::Playing,
            None if self.loading.is_some() => Status::Loading,
            None if self.waiting_start => Status::Paused,
            None => Status::Idle,
        };
        let paused_reason = (status == Status::Paused).then(|| {
            if self.empty.is_waiting() {
                PausedReason::EmptyChannel
            } else {
                PausedReason::User
            }
        });
        let rule = self.settings.vote_skip;
        PlayerSnapshot {
            guild_id: self.guild_id,
            channel_id: self.voice_channel_id,
            text_channel_id: self.text_channel,
            bot_index: self.bot.index,
            bot_user_id: self.bot.user_id(),
            status,
            paused_reason,
            empty_channel_deadline: self.empty.deadline().map(deadline_epoch_ms),
            current: current.clone(),
            position_ms: self.playing.as_ref().map(Playing::position_ms).unwrap_or(0),
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

    /// Queue the session write (debounced by the writer). `tracks`: the track list changed.
    fn persist(&self, tracks: bool) {
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
            updated_at: 0,
        };
        let list = tracks.then(|| self.queue.all().iter().map(|e| e.to_session()).collect());
        self.deps.sessions.save(record, list);
    }

    async fn finish(&mut self, reason: EndReason) {
        info!(
            guild = self.guild_id,
            channel = self.voice_channel_id,
            ?reason,
            "player ending"
        );
        let restore = Restore {
            tracks: self.queue.all().to_vec(),
            index: self.queue.index().min(self.queue.all().len().saturating_sub(1)),
            loop_mode: self.queue.loop_mode,
            volume: self.volume,
            after_restart: false,
        };
        if let Some(r) = self.radio.as_mut() {
            r.stop();
        }
        if let Some(p) = self.playing.take() {
            let _ = p.handle.stop();
            if reason != EndReason::Shutdown {
                self.record_play(&p);
            }
        }
        self.prefetched = None;

        if reason == EndReason::Shutdown {
            self.persist(true);
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
            self.deps.sources.stream_dirs().remove_channel(self.guild_id, c).await;
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
