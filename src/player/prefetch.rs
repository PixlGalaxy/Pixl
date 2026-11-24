//! Look-ahead prefetching for one player: which upcoming tracks to prepare, and when.
//!
//! While a track plays, the next `PREFETCH_TRACKS` tracks of the queue (what skips would reach)
//! are prepared one at a time: the first [`FIRST_AFTER_START`] after the track started, each
//! following one [`STAGGER`] after the previous finished, so a player never runs several yt-dlp
//! resolves at once. A prefetch is the stream URL (`Sources::prepare`) plus, for direct streams,
//! the first ~10 s of audio on disk (see `heads`).
//!
//! Queue edits are debounced ([`DEBOUNCE`]): only once the queue stopped changing is the wanted
//! set recomputed, prefetches that left it discarded (their dirs and heads deleted) and new ones
//! started. A prefetched URL is used only with [`MIN_URL_LEFT_SECS`] left; one that would expire
//! before its track plausibly starts is resolved again (its head is kept when the encode matches).
//!
//! This module only plans; the actor runs the jobs and applies the [`Plan`]s.

use std::{collections::HashSet, time::Duration};

use tokio::time::Instant;
use tracing::debug;
use uuid::Uuid;

use crate::sources::{Playable, SourceError};

/// First prefetch after a track started (or resumed): its own start comes first.
pub const FIRST_AFTER_START: Duration = Duration::from_secs(3);
/// Between the end of one prefetch and the start of the next.
pub const STAGGER: Duration = Duration::from_secs(3);
/// Quiet time after a queue edit before the wanted tracks are recomputed.
pub const DEBOUNCE: Duration = Duration::from_secs(2);
/// A prefetched URL needs this much life left to be used.
pub const MIN_URL_LEFT_SECS: i64 = 10 * 60;
/// Ready prefetches are re-checked against their URL expiry this often while playing.
pub const REFRESH_EVERY: Duration = Duration::from_secs(5 * 60);
/// A job that never reported back (lost message) is forgotten after this long.
pub const JOB_MAX: Duration = Duration::from_secs(150);
/// Refresh only URLs the stream cache will not hand out again (it keeps URLs with more than
/// this left), so a refresh really gets a new URL.
const REFRESH_BELOW_SECS: i64 = crate::sources::ytdlp::STREAM_MIN_REMAINING;

/// What this player prefetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Its own queue: URLs and heads.
    Own,
    /// Jam host: URLs and heads of the shared queue, for every member.
    Host,
    /// Jam member: nothing; it reuses the host's URLs (stream cache) and heads.
    Member,
}

impl Role {
    pub fn fetches(self) -> bool {
        self != Role::Member
    }
}

/// An upcoming track, in play order.
#[derive(Debug, Clone, Copy)]
pub struct Want {
    pub uid: Uuid,
    pub duration_ms: Option<u64>,
}

struct Ready {
    uid: Uuid,
    playable: Playable,
}

#[derive(Debug, Clone, Copy)]
struct Running {
    uid: Uuid,
    job: u64,
    started: Instant,
    /// Its URL came back (the head may still be downloading).
    resolved: bool,
    /// The track was asked to play while this ran: its result starts it.
    claimed: bool,
}

/// What the actor should do after [`Prefetcher::tick`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Prefetches dropped: kill their processes and delete their dirs (unless the track plays).
    pub discard: Vec<Uuid>,
    /// Abort this job's task.
    pub abort: Option<u64>,
    /// Start prefetching this track as this job.
    pub start: Option<(Uuid, u64)>,
    /// The track waiting for a job that was lost: prepare it normally.
    pub retry: Option<Uuid>,
}

/// [`Prefetcher::take`] when a track is about to play.
pub enum Taken {
    /// Prefetched with enough URL life left.
    Fresh(Box<Playable>),
    /// Prefetched, but the URL expires too soon: resolve it again.
    Stale,
    /// Still being prefetched: its result will start the track.
    Pending,
    None,
}

/// [`Prefetcher::resolved`]: what to do with a job's prepare result.
pub enum Resolved {
    /// Kept for when the track plays (or recorded as failed).
    Stored,
    /// The track is waiting for it.
    Claimed(Box<Result<Playable, SourceError>>),
    /// From a job that is no longer running.
    Ignored,
}

pub struct Prefetcher {
    depth: usize,
    /// The current track runs (prefetching only happens then).
    active: bool,
    ready: Vec<Ready>,
    running: Option<Running>,
    /// Prepares that failed: not retried until they leave the wanted set (they are prepared
    /// normally when they play).
    failed: HashSet<Uuid>,
    next_at: Option<Instant>,
    edit_at: Option<Instant>,
    refresh_at: Option<Instant>,
    last_done: Option<Instant>,
    jobs: u64,
}

impl Prefetcher {
    pub fn new(depth: usize) -> Self {
        Self {
            depth,
            active: false,
            ready: Vec::new(),
            running: None,
            failed: HashSet::new(),
            next_at: None,
            edit_at: None,
            refresh_at: None,
            last_done: None,
            jobs: 0,
        }
    }

    /// When [`Self::tick`] is due. While an edit is being debounced, only its end.
    pub fn deadline(&self) -> Option<Instant> {
        if self.depth == 0 {
            return None;
        }
        if let Some(e) = self.edit_at {
            return Some(e);
        }
        // A track waits for this job: notice if it never reports back.
        let lost_at = self.running.filter(|r| r.claimed).map(|r| r.started + JOB_MAX);
        let mut due = [lost_at, None, None];
        if self.active {
            due[1] = self.next_at;
            due[2] = self.refresh_at.filter(|_| !self.ready.is_empty());
        }
        due.into_iter().flatten().min()
    }

    /// A new track started (`running`: not paused).
    pub fn track_started(&mut self, now: Instant, running: bool) {
        self.active = running;
        self.next_at = running.then(|| now + FIRST_AFTER_START);
        self.refresh_at = running.then(|| now + REFRESH_EVERY);
    }

    /// The current track was paused or resumed.
    pub fn set_running(&mut self, now: Instant, running: bool) {
        if running == self.active {
            return;
        }
        self.active = running;
        if running {
            self.next_at = Some(now + FIRST_AFTER_START);
            self.refresh_at = Some(now + REFRESH_EVERY);
        } else {
            self.next_at = None;
        }
    }

    /// Nothing plays (track ended, queue over): no new prefetches until the next track starts.
    pub fn stopped(&mut self) {
        self.active = false;
        self.next_at = None;
    }

    /// The queue (or loop mode, or the jam role) changed: recompute once it settles.
    pub fn queue_changed(&mut self, now: Instant) {
        if self.depth > 0 {
            self.edit_at = Some(now + DEBOUNCE);
        }
    }

    /// Bring the prefetches in line with `wanted` (the next tracks, in order) and start the next
    /// job when one is due. `left_ms`: what remains of the current track; `now_secs`: Unix time.
    pub fn tick(&mut self, now: Instant, wanted: &[Want], left_ms: u64, now_secs: i64, role: Role) -> Plan {
        let mut plan = Plan::default();
        if self.edit_at.is_some_and(|e| e > now) {
            return plan;
        }
        let edited = self.edit_at.take().is_some();
        let wanted: &[Want] = if role.fetches() {
            &wanted[..wanted.len().min(self.depth)]
        } else {
            &[]
        };
        let is_wanted = |uid: Uuid| wanted.iter().any(|w| w.uid == uid);

        self.ready.retain(|r| {
            let keep = is_wanted(r.uid);
            if !keep {
                debug!(uid = %r.uid, "prefetch: no longer among the next tracks, discarding it");
                plan.discard.push(r.uid);
            }
            keep
        });
        if let Some(r) = self.running {
            let lost = now.saturating_duration_since(r.started) > JOB_MAX;
            let unwanted = !r.claimed && !is_wanted(r.uid);
            if lost || unwanted {
                debug!(uid = %r.uid, job = r.job, lost, "prefetch: stopping a running prefetch");
                plan.abort = Some(r.job);
                if unwanted && !plan.discard.contains(&r.uid) {
                    plan.discard.push(r.uid);
                }
                if r.claimed {
                    plan.retry = Some(r.uid);
                }
                self.running = None;
            }
        }
        self.failed.retain(|u| is_wanted(*u));

        // URLs that would expire before their track plausibly starts: resolve them again.
        if self.refresh_at.is_some_and(|t| t <= now) {
            self.refresh_at = Some(now + REFRESH_EVERY);
        }
        let mut eta_ms = left_ms;
        for w in wanted {
            if let Some(i) = self.ready.iter().position(|r| r.uid == w.uid) {
                let left = self.ready[i].playable.stream.expires_at - now_secs;
                let eta = (eta_ms / 1000) as i64;
                if left < eta + MIN_URL_LEFT_SECS && left <= REFRESH_BELOW_SECS {
                    debug!(uid = %w.uid, url_left_secs = left, eta_secs = eta, "prefetch: URL would expire before the track plays; resolving it again");
                    // Not discarded: the dir (and a head of the same encode) stays.
                    self.ready.remove(i);
                }
            }
            eta_ms += w.duration_ms.unwrap_or(0);
        }

        if !self.active {
            self.next_at = None;
            return plan;
        }
        if edited && self.next_at.is_none() {
            let earliest = self.last_done.map_or(now, |d| (d + STAGGER).max(now));
            self.next_at = Some(earliest);
        }
        if self.next_at.is_none_or(|t| t > now) {
            return plan;
        }
        if self.running.is_some() {
            // `done` schedules the next one.
            self.next_at = None;
            return plan;
        }
        self.next_at = None;
        let next = wanted
            .iter()
            .map(|w| w.uid)
            .find(|u| !self.failed.contains(u) && !self.ready.iter().any(|r| r.uid == *u));
        if let Some(uid) = next {
            self.jobs += 1;
            let job = self.jobs;
            debug!(%uid, job, ?role, "prefetch: starting");
            self.running = Some(Running {
                uid,
                job,
                started: now,
                resolved: false,
                claimed: false,
            });
            plan.start = Some((uid, job));
        }
        plan
    }

    /// The prepare of `job` finished.
    pub fn resolved(&mut self, job: u64, uid: Uuid, result: Result<Playable, SourceError>) -> Resolved {
        let Some(r) = self.running.as_mut().filter(|r| r.job == job && r.uid == uid) else {
            return Resolved::Ignored;
        };
        r.resolved = true;
        if r.claimed {
            return Resolved::Claimed(Box::new(result));
        }
        match result {
            Ok(p) => self.store(uid, p),
            Err(e) => {
                debug!(%uid, error = %e, "prefetch failed; the track is prepared when it plays");
                self.failed.insert(uid);
            }
        }
        Resolved::Stored
    }

    /// Keep `playable` for when `uid` plays.
    pub fn store(&mut self, uid: Uuid, playable: Playable) {
        self.ready.retain(|r| r.uid != uid);
        self.ready.push(Ready { uid, playable });
    }

    /// `job` is completely done (URL and head): the next one may start after [`STAGGER`].
    pub fn done(&mut self, job: u64, now: Instant) {
        if self.running.is_some_and(|r| r.job == job) {
            self.running = None;
            self.last_done = Some(now);
            if self.active {
                self.next_at = Some(now + STAGGER);
            }
        }
    }

    /// `uid` is about to play: hand over its prefetch (checking the URL life left).
    pub fn take(&mut self, uid: Uuid, now_secs: i64) -> Taken {
        if let Some(i) = self.ready.iter().position(|r| r.uid == uid) {
            let r = self.ready.remove(i);
            let left = r.playable.stream.expires_at - now_secs;
            if left >= MIN_URL_LEFT_SECS {
                return Taken::Fresh(Box::new(r.playable));
            }
            debug!(%uid, url_left_secs = left, "prefetch: URL expires too soon, resolving it again");
            return Taken::Stale;
        }
        if let Some(r) = self.running.as_mut().filter(|r| r.uid == uid && !r.resolved) {
            debug!(%uid, job = r.job, "prefetch: track asked to play while it is prefetched; waiting for it");
            r.claimed = true;
            return Taken::Pending;
        }
        Taken::None
    }

    /// Drop every prefetch (jam join / leave, player end).
    pub fn clear(&mut self) -> Plan {
        let mut plan = Plan::default();
        plan.discard.extend(self.ready.drain(..).map(|r| r.uid));
        if let Some(r) = self.running.take() {
            plan.abort = Some(r.job);
            if !plan.discard.contains(&r.uid) {
                plan.discard.push(r.uid);
            }
        }
        self.failed.clear();
        self.next_at = None;
        self.edit_at = None;
        plan
    }

    #[cfg(test)]
    fn ready_uids(&self) -> Vec<Uuid> {
        self.ready.iter().map(|r| r.uid).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::{PrepareTimings, StreamInfo, TrackInfo, TrackOrigin};

    const NOW: i64 = 1_700_000_000;

    fn playable(expires_at: i64) -> Playable {
        Playable {
            track: TrackInfo {
                title: "t".into(),
                artists: Vec::new(),
                duration_ms: Some(180_000),
                thumbnail: None,
                url: "https://www.youtube.com/watch?v=x".into(),
                origin: TrackOrigin::Youtube,
                youtube_id: Some("x".into()),
                spotify_id: None,
                isrc: None,
                music: None,
            },
            stream: StreamInfo {
                url: "https://x.googlevideo.com/videoplayback?itag=251&clen=1000".into(),
                http_headers: Vec::new(),
                expires_at,
                container: Some("webm".into()),
                codec: Some("opus".into()),
                format_id: Some("251".into()),
                protocol: Some("https".into()),
                is_opus: true,
                needs_ytdlp: false,
                duration_ms: Some(180_000),
                available_at: None,
            },
            ytdlp_fallback: false,
            page_url: "https://www.youtube.com/watch?v=x".into(),
            work_dir: None,
            played_youtube_id: Some("x".into()),
            fallback_reason: None,
            late_music: None,
            timings: PrepareTimings::new(tokio::time::Instant::now()),
        }
    }

    fn wants(uids: &[Uuid]) -> Vec<Want> {
        uids.iter()
            .map(|u| Want {
                uid: *u,
                duration_ms: Some(180_000),
            })
            .collect()
    }

    fn uids(n: usize) -> Vec<Uuid> {
        (0..n).map(|_| Uuid::new_v4()).collect()
    }

    /// Run a job to completion: URL, then the head.
    fn finish(p: &mut Prefetcher, plan: &Plan, at: Instant, expires_at: i64) {
        let (uid, job) = plan.start.expect("a job started");
        assert!(matches!(p.resolved(job, uid, Ok(playable(expires_at))), Resolved::Stored));
        p.done(job, at);
    }

    #[test]
    fn prefetches_the_next_n_one_at_a_time_staggered() {
        let t0 = Instant::now();
        let ids = uids(4);
        let w = wants(&ids);
        let mut p = Prefetcher::new(2);
        p.track_started(t0, true);
        assert_eq!(p.deadline(), Some(t0 + FIRST_AFTER_START));
        // Nothing before the first delay.
        assert_eq!(p.tick(t0 + Duration::from_secs(1), &w, 100_000, NOW, Role::Own), Plan::default());

        let t1 = t0 + FIRST_AFTER_START;
        let plan = p.tick(t1, &w, 100_000, NOW, Role::Own);
        assert_eq!(plan.start.map(|s| s.0), Some(ids[0]));
        // While it runs nothing else starts, and the deadline is not in the past (no busy loop).
        assert!(p.deadline().is_none_or(|d| d > t1));
        assert_eq!(p.tick(t1, &w, 100_000, NOW, Role::Own).start, None);

        let t2 = t1 + Duration::from_secs(2);
        finish(&mut p, &plan, t2, NOW + 6 * 3600);
        assert_eq!(p.deadline(), Some(t2 + STAGGER));
        assert_eq!(p.tick(t2 + Duration::from_secs(1), &w, 100_000, NOW, Role::Own).start, None);
        let plan = p.tick(t2 + STAGGER, &w, 100_000, NOW, Role::Own);
        assert_eq!(plan.start.map(|s| s.0), Some(ids[1]));
        let t3 = t2 + STAGGER + Duration::from_secs(1);
        finish(&mut p, &plan, t3, NOW + 6 * 3600);
        // Depth 2: the third track is not prefetched.
        let plan = p.tick(t3 + STAGGER, &w, 100_000, NOW, Role::Own);
        assert_eq!(plan, Plan::default());
        assert_eq!(p.ready_uids(), vec![ids[0], ids[1]]);
        assert!(matches!(p.take(ids[0], NOW), Taken::Fresh(_)));
    }

    #[test]
    fn paused_players_do_not_prefetch() {
        let t0 = Instant::now();
        let ids = uids(2);
        let mut p = Prefetcher::new(2);
        p.track_started(t0, false);
        assert_eq!(p.deadline(), None);
        p.track_started(t0, true);
        p.set_running(t0, false);
        assert_eq!(p.deadline(), None);
        p.set_running(t0 + Duration::from_secs(60), true);
        assert_eq!(p.deadline(), Some(t0 + Duration::from_secs(60) + FIRST_AFTER_START));
        let plan = p.tick(t0 + Duration::from_secs(63), &wants(&ids), 0, NOW, Role::Own);
        assert_eq!(plan.start.map(|s| s.0), Some(ids[0]));
    }

    #[test]
    fn depth_zero_disables_it() {
        let t0 = Instant::now();
        let mut p = Prefetcher::new(0);
        p.track_started(t0, true);
        p.queue_changed(t0);
        assert_eq!(p.deadline(), None);
        assert_eq!(p.tick(t0 + Duration::from_secs(10), &wants(&uids(2)), 0, NOW, Role::Own), Plan::default());
    }

    /// Rapid edits (drag reordering) only recompute once the queue has been quiet for the debounce;
    /// then prefetches that left the next N are discarded and the new ones are started.
    #[test]
    fn queue_edits_are_debounced_then_recomputed() {
        let t0 = Instant::now();
        let ids = uids(4);
        let mut p = Prefetcher::new(2);
        p.track_started(t0, true);
        let t = t0 + FIRST_AFTER_START;
        let plan = p.tick(t, &wants(&ids[..2]), 0, NOW, Role::Own);
        finish(&mut p, &plan, t, NOW + 6 * 3600);
        let t = t + STAGGER;
        let plan = p.tick(t, &wants(&ids[..2]), 0, NOW, Role::Own);
        finish(&mut p, &plan, t, NOW + 6 * 3600);
        assert_eq!(p.ready_uids(), vec![ids[0], ids[1]]);

        // Five edits 500 ms apart: each pushes the recompute back.
        let mut at = t + Duration::from_secs(10);
        for _ in 0..5 {
            p.queue_changed(at);
            at += Duration::from_millis(500);
            assert_eq!(p.tick(at, &wants(&[ids[2], ids[0]]), 0, NOW, Role::Own), Plan::default());
        }
        assert_eq!(p.deadline(), Some(at - Duration::from_millis(500) + DEBOUNCE));
        let settled = at + DEBOUNCE;
        let plan = p.tick(settled, &wants(&[ids[2], ids[0]]), 0, NOW, Role::Own);
        assert_eq!(plan.discard, vec![ids[1]]);
        assert_eq!(plan.start.map(|s| s.0), Some(ids[2]), "the new next track starts right away");
        assert_eq!(p.ready_uids(), vec![ids[0]]);

        // Removing a track that is being prefetched aborts its job.
        let (uid, job) = plan.start.unwrap();
        p.queue_changed(settled);
        let plan = p.tick(settled + DEBOUNCE, &wants(&[ids[0]]), 0, NOW, Role::Own);
        assert_eq!(plan.abort, Some(job));
        assert_eq!(plan.discard, vec![uid]);
        // Its late result is ignored.
        assert!(matches!(p.resolved(job, uid, Ok(playable(NOW + 3600))), Resolved::Ignored));
    }

    #[test]
    fn expiring_urls_are_revalidated() {
        let t0 = Instant::now();
        let ids = uids(2);
        let mut p = Prefetcher::new(2);
        p.track_started(t0, true);
        let t = t0 + FIRST_AFTER_START;
        let plan = p.tick(t, &wants(&ids), 0, NOW, Role::Own);
        finish(&mut p, &plan, t, NOW + 9 * 60);
        // Less than 10 minutes left when it is about to play: not used.
        assert!(matches!(p.take(ids[0], NOW), Taken::Stale));

        let plan = p.tick(t + STAGGER, &wants(&ids[1..]), 0, NOW, Role::Own);
        finish(&mut p, &plan, t + STAGGER, NOW + 25 * 60);
        // Fine now, but the current track lasts 20 more minutes: refreshed by a later tick
        // (dropped from the ready set without deleting its dir, then prefetched again).
        let later = t + STAGGER + REFRESH_EVERY;
        let plan = p.tick(later, &wants(&ids[1..]), 20 * 60 * 1000, NOW, Role::Own);
        assert!(plan.discard.is_empty());
        assert_eq!(plan.start.map(|s| s.0), Some(ids[1]), "prefetched again");
        // Plenty left (6 h): kept however long the wait.
        finish(&mut p, &plan, later + STAGGER, NOW + 6 * 3600);
        p.tick(later + STAGGER + REFRESH_EVERY, &wants(&ids[1..]), 60 * 60 * 1000, NOW, Role::Own);
        assert_eq!(p.ready_uids(), vec![ids[1]]);
        assert!(matches!(p.take(ids[1], NOW), Taken::Fresh(_)));
    }

    /// Jam members never start prefetches (and drop what they had when they became members).
    #[test]
    fn jam_members_do_not_prefetch() {
        let t0 = Instant::now();
        let ids = uids(2);
        let mut p = Prefetcher::new(2);
        p.track_started(t0, true);
        let t = t0 + FIRST_AFTER_START;
        let plan = p.tick(t, &wants(&ids), 0, NOW, Role::Host);
        finish(&mut p, &plan, t, NOW + 3600);
        p.queue_changed(t);
        let plan = p.tick(t + DEBOUNCE, &wants(&ids), 0, NOW, Role::Member);
        assert_eq!(plan.discard, vec![ids[0]]);
        assert_eq!(plan.start, None);
        assert_eq!(p.tick(t + DEBOUNCE + STAGGER * 3, &wants(&ids), 0, NOW, Role::Member).start, None);
        // A member from the start never fetches either.
        let mut m = Prefetcher::new(2);
        m.track_started(t0, true);
        assert_eq!(m.tick(t, &wants(&ids), 0, NOW, Role::Member), Plan::default());
        assert_eq!(m.deadline(), None);
    }

    /// A track asked to play while it is being prefetched waits for that job instead of a second
    /// resolve; the job is never discarded meanwhile.
    #[test]
    fn a_running_prefetch_can_be_claimed() {
        let t0 = Instant::now();
        let ids = uids(2);
        let mut p = Prefetcher::new(2);
        p.track_started(t0, true);
        let t = t0 + FIRST_AFTER_START;
        let plan = p.tick(t, &wants(&ids), 0, NOW, Role::Own);
        let (uid, job) = plan.start.unwrap();
        assert!(matches!(p.take(uid, NOW), Taken::Pending));
        // It became the current track, so it is not wanted anymore: still kept.
        p.queue_changed(t);
        let plan = p.tick(t + DEBOUNCE, &wants(&ids[1..]), 0, NOW, Role::Own);
        assert!(plan.discard.is_empty() && plan.abort.is_none());
        assert!(matches!(p.resolved(job, uid, Ok(playable(NOW + 3600))), Resolved::Claimed(_)));
        assert!(p.ready_uids().is_empty());
        p.done(job, t + DEBOUNCE);

        // A claimed job that never reports back: the waiting track is prepared normally.
        let plan = p.tick(t + DEBOUNCE + STAGGER, &wants(&ids[1..]), 0, NOW, Role::Own);
        let (uid, _) = plan.start.unwrap();
        assert!(matches!(p.take(uid, NOW), Taken::Pending));
        p.stopped();
        assert!(p.deadline().is_some(), "a waiting track keeps the lost-job check armed");
        let plan = p.tick(t + JOB_MAX * 2, &[], 0, NOW, Role::Own);
        assert_eq!(plan.retry, Some(uid));
        assert!(plan.discard.is_empty());
    }

    #[test]
    fn failed_prefetches_are_not_retried_and_clear_drops_everything() {
        let t0 = Instant::now();
        let ids = uids(2);
        let mut p = Prefetcher::new(2);
        p.track_started(t0, true);
        let t = t0 + FIRST_AFTER_START;
        let plan = p.tick(t, &wants(&ids), 0, NOW, Role::Own);
        let (uid, job) = plan.start.unwrap();
        assert!(matches!(p.resolved(job, uid, Err(SourceError::Timeout)), Resolved::Stored));
        p.done(job, t);
        let plan = p.tick(t + STAGGER, &wants(&ids), 0, NOW, Role::Own);
        assert_eq!(plan.start.map(|s| s.0), Some(ids[1]), "the failed one is skipped");
        let cleared = p.clear();
        assert_eq!(cleared.abort, plan.start.map(|s| s.1));
        assert_eq!(cleared.discard, vec![ids[1]]);
        assert_eq!(p.deadline(), None);
    }

    /// A job that never reports back does not block prefetching for good.
    #[test]
    fn lost_jobs_are_forgotten() {
        let t0 = Instant::now();
        let ids = uids(2);
        let mut p = Prefetcher::new(2);
        p.track_started(t0, true);
        let plan = p.tick(t0 + FIRST_AFTER_START, &wants(&ids), 0, NOW, Role::Own);
        let job = plan.start.unwrap().1;
        let later = t0 + JOB_MAX + Duration::from_secs(10);
        p.track_started(later, true);
        let plan = p.tick(later + FIRST_AFTER_START, &wants(&ids), 0, NOW, Role::Own);
        assert_eq!(plan.abort, Some(job));
        assert_eq!(plan.start.map(|s| s.0), Some(ids[0]));
    }
}
