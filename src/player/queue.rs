//! Queue model of one player: the full track list (played + upcoming), the current index and the
//! loop mode. Pure data, no I/O; every operation is unit-tested.
//!
//! Positions given by users (commands) are 1-based within the upcoming list.

use std::sync::Arc;

use uuid::Uuid;

use crate::{
    sources::{TrackInfo, TrackSource},
    store::{LoopMode, SessionTrack},
};

/// Played tracks kept before the current one (history, `previous`) when not looping the queue.
pub const MAX_PLAYED_KEPT: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requester {
    pub id: u64,
    /// Display name at the time of the request (unknown for tracks restored after a restart).
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

impl Requester {
    pub fn id_only(id: u64) -> Self {
        Self {
            id,
            name: None,
            avatar_url: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub uid: Uuid,
    pub info: TrackInfo,
    pub requester: Requester,
    /// AI radio: why this track was picked (`{ key, params }`), shown by the panel.
    pub reason: Option<serde_json::Value>,
    /// The playlist / album this entry was added from (None for single tracks and radio picks).
    pub source: Option<TrackSource>,
}

impl QueueEntry {
    pub fn new(info: TrackInfo, requester: Requester) -> Self {
        Self {
            uid: Uuid::new_v4(),
            info,
            requester,
            reason: None,
            source: None,
        }
    }

    pub fn with_source(mut self, source: Option<TrackSource>) -> Self {
        self.source = source;
        self
    }

    pub fn to_session(&self) -> SessionTrack {
        SessionTrack {
            uid: self.uid.to_string(),
            info: self.info.clone(),
            requested_by: self.requester.id,
            source: self.source.clone(),
        }
    }

    pub fn from_session(t: &SessionTrack) -> Self {
        Self {
            uid: Uuid::parse_str(&t.uid).unwrap_or_else(|_| Uuid::new_v4()),
            info: t.info.clone(),
            requester: Requester::id_only(t.requested_by),
            reason: None,
            source: t.source.clone(),
        }
    }
}

pub type Entry = Arc<QueueEntry>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// The upcoming list is already at the limit.
    Full { max: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushResult {
    pub added: usize,
    /// 1-based upcoming position of the first added track; 0 when it becomes the current track.
    pub first_position: usize,
    /// Some tracks were dropped to respect the limit.
    pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Queue {
    tracks: Vec<Entry>,
    /// Index of the current track when `active`, otherwise of the next track to start.
    pos: usize,
    active: bool,
    pub loop_mode: LoopMode,
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore a saved queue; nothing is active until `start`.
    pub fn restore(tracks: Vec<Entry>, index: usize, loop_mode: LoopMode) -> Self {
        let pos = index.min(tracks.len());
        Self {
            tracks,
            pos,
            active: false,
            loop_mode,
        }
    }

    /// A copy of another queue's state (a jam member follows the shared queue).
    pub fn mirror(tracks: Vec<Entry>, index: usize, active: bool, loop_mode: LoopMode) -> Self {
        let pos = index.min(tracks.len());
        Self {
            active: active && pos < tracks.len(),
            tracks,
            pos,
            loop_mode,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn current(&self) -> Option<&Entry> {
        if self.active {
            self.tracks.get(self.pos)
        } else {
            None
        }
    }

    /// The entry `start` would play (current when active).
    pub fn cursor(&self) -> Option<&Entry> {
        self.tracks.get(self.pos)
    }

    pub fn upcoming(&self) -> &[Entry] {
        let from = if self.active { self.pos + 1 } else { self.pos };
        self.tracks.get(from..).unwrap_or(&[])
    }

    fn upcoming_start(&self) -> usize {
        if self.active {
            self.pos + 1
        } else {
            self.pos
        }
    }

    /// The last `n` played tracks, most recent first.
    pub fn history(&self, n: usize) -> Vec<Entry> {
        self.tracks[..self.pos.min(self.tracks.len())]
            .iter()
            .rev()
            .take(n)
            .cloned()
            .collect()
    }

    /// Whole list (played + current + upcoming) and the index saved with the session.
    pub fn all(&self) -> &[Entry] {
        &self.tracks
    }

    pub fn index(&self) -> usize {
        self.pos
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.current().is_none() && self.upcoming().is_empty()
    }

    /// Append tracks, keeping at most `max` upcoming ones. Returns the 1-based upcoming position of
    /// the first one (0 when it is the next track to start on an idle queue).
    pub fn push(&mut self, entries: Vec<Entry>, max: usize) -> Result<PushResult, PushError> {
        let room = max.saturating_sub(self.upcoming().len());
        if room == 0 {
            return Err(PushError::Full { max });
        }
        let truncated = entries.len() > room;
        let first_index = self.tracks.len();
        let added = entries.len().min(room);
        self.tracks.extend(entries.into_iter().take(room));
        // When inactive the cursor track starts next and counts as the current one (position 0).
        let first_position = first_index - self.pos.min(first_index);
        Ok(PushResult {
            added,
            first_position,
            truncated,
        })
    }

    /// Start the cursor track when idle. Returns the new current track.
    pub fn start(&mut self) -> Option<Entry> {
        if self.active {
            return self.current().cloned();
        }
        let e = self.tracks.get(self.pos).cloned()?;
        self.active = true;
        Some(e)
    }

    /// Move on after the current track ended (`skip: false`) or was skipped (`skip: true`).
    /// Returns the next current track, or None when the queue is over (then inactive).
    pub fn advance(&mut self, skip: bool) -> Option<Entry> {
        if !self.active {
            return self.start();
        }
        if self.loop_mode == LoopMode::Track && !skip {
            return self.current().cloned();
        }
        self.pos += 1;
        if self.pos >= self.tracks.len() {
            if self.loop_mode == LoopMode::Queue && !self.tracks.is_empty() {
                self.pos = 0;
            } else {
                self.pos = self.tracks.len();
                self.active = false;
                self.trim_played();
                return None;
            }
        }
        self.trim_played();
        self.current().cloned()
    }

    /// The track that will play after the current one ends normally (prefetch).
    pub fn peek_next(&self) -> Option<&Entry> {
        if !self.active {
            return self.cursor();
        }
        match self.loop_mode {
            LoopMode::Track => None,
            LoopMode::Queue if self.pos + 1 >= self.tracks.len() => self.tracks.first(),
            _ => self.tracks.get(self.pos + 1),
        }
    }

    /// The next `n` distinct tracks that skips (or the current track ending) reach, in order, without
    /// the current one. A track loop still skips to the following tracks; a queue loop wraps.
    pub fn peek_ahead(&self, n: usize) -> Vec<Entry> {
        let len = self.tracks.len();
        let current = self.current().map(|e| e.uid);
        let mut out: Vec<Entry> = Vec::new();
        let mut i = self.upcoming_start();
        for _ in 0..len {
            if out.len() >= n {
                break;
            }
            if i >= len {
                if self.loop_mode != LoopMode::Queue {
                    break;
                }
                i = 0;
            }
            let e = &self.tracks[i];
            if Some(e.uid) != current && !out.iter().any(|o| o.uid == e.uid) {
                out.push(e.clone());
            }
            i += 1;
        }
        out
    }

    /// Go back one track. Returns the new current track.
    pub fn previous(&mut self) -> Option<Entry> {
        if self.pos == 0 || self.tracks.is_empty() {
            return None;
        }
        self.pos = (self.pos - 1).min(self.tracks.len() - 1);
        self.active = true;
        self.current().cloned()
    }

    fn trim_played(&mut self) {
        if self.loop_mode == LoopMode::Queue || self.pos <= MAX_PLAYED_KEPT {
            return;
        }
        let cut = self.pos - MAX_PLAYED_KEPT;
        self.tracks.drain(..cut);
        self.pos -= cut;
    }

    fn upcoming_index(&self, position: usize) -> Option<usize> {
        if position == 0 || position > self.upcoming().len() {
            return None;
        }
        Some(self.upcoming_start() + position - 1)
    }

    /// Remove the track at a 1-based upcoming position.
    pub fn remove(&mut self, position: usize) -> Option<Entry> {
        let i = self.upcoming_index(position)?;
        Some(self.tracks.remove(i))
    }

    /// Remove an upcoming track by uid (the current track cannot be removed; skip it instead).
    pub fn remove_uid(&mut self, uid: Uuid) -> Option<Entry> {
        let pos = self.position_of(uid)?;
        self.remove(pos)
    }

    /// 1-based upcoming position of a uid.
    pub fn position_of(&self, uid: Uuid) -> Option<usize> {
        self.upcoming().iter().position(|e| e.uid == uid).map(|i| i + 1)
    }

    /// Move an upcoming track between 1-based positions (`to` is clamped to the end).
    pub fn move_track(&mut self, from: usize, to: usize) -> Option<(Entry, usize)> {
        let len = self.upcoming().len();
        let from_i = self.upcoming_index(from)?;
        let to = to.clamp(1, len);
        let entry = self.tracks.remove(from_i);
        let to_i = self.upcoming_start() + to - 1;
        self.tracks.insert(to_i, entry.clone());
        Some((entry, to))
    }

    /// Jump to the track at a 1-based upcoming position, dropping the tracks before it.
    /// Returns the dropped tracks and the new current track.
    pub fn skip_to(&mut self, position: usize) -> Option<(Vec<Entry>, Entry)> {
        let target = self.upcoming_index(position)?;
        let start = self.upcoming_start();
        let dropped: Vec<Entry> = self.tracks.drain(start..target).collect();
        let next = if self.active {
            self.advance(true)?
        } else {
            self.start()?
        };
        Some((dropped, next))
    }

    /// Shuffle the upcoming tracks (the order really changes; it is not a random pick per track).
    pub fn shuffle(&mut self, rng: &mut fastrand::Rng) -> usize {
        let start = self.upcoming_start().min(self.tracks.len());
        let slice = &mut self.tracks[start..];
        rng.shuffle(slice);
        slice.len()
    }

    /// Drop every upcoming track; returns them.
    pub fn clear_upcoming(&mut self) -> Vec<Entry> {
        let start = self.upcoming_start().min(self.tracks.len());
        self.tracks.drain(start..).collect()
    }

    /// Replace an entry with an updated copy (e.g. after the lazy Spotify match).
    pub fn update(&mut self, entry: Entry) {
        if let Some(slot) = self.tracks.iter_mut().find(|e| e.uid == entry.uid) {
            *slot = entry;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_util::track;

    fn entries(n: usize) -> Vec<Entry> {
        (0..n)
            .map(|i| Arc::new(QueueEntry::new(track(&format!("t{i}")), Requester::id_only(1))))
            .collect()
    }

    fn titles(list: &[Entry]) -> Vec<String> {
        list.iter().map(|e| e.info.youtube_id.clone().unwrap()).collect()
    }

    fn cur(q: &Queue) -> String {
        q.current()
            .map(|e| e.info.youtube_id.clone().unwrap())
            .unwrap_or_default()
    }

    #[test]
    fn peek_ahead_follows_skips_and_loops() {
        let mut q = Queue::new();
        q.push(entries(4), 10).unwrap();
        q.start();
        assert_eq!(titles(&q.peek_ahead(2)), vec!["t1", "t2"]);
        assert_eq!(titles(&q.peek_ahead(10)), vec!["t1", "t2", "t3"]);
        assert!(q.peek_ahead(0).is_empty());
        q.advance(false);
        q.advance(false);
        assert_eq!(titles(&q.peek_ahead(2)), vec!["t3"], "end of the queue");
        q.loop_mode = LoopMode::Queue;
        assert_eq!(titles(&q.peek_ahead(2)), vec!["t3", "t0"], "wraps");
        assert_eq!(titles(&q.peek_ahead(9)), vec!["t3", "t0", "t1"], "never the current one, no repeats");
        q.loop_mode = LoopMode::Track;
        assert_eq!(titles(&q.peek_ahead(2)), vec!["t3"], "a skip leaves the looped track");
    }

    #[test]
    fn push_start_and_advance() {
        let mut q = Queue::new();
        let r = q.push(entries(3), 10).unwrap();
        assert_eq!(
            r,
            PushResult {
                added: 3,
                first_position: 0,
                truncated: false
            }
        );
        assert!(q.current().is_none());
        assert_eq!(cur_after(&mut q), "t0");
        assert_eq!(titles(q.upcoming()), vec!["t1", "t2"]);
        let r = q.push(entries(1), 10).unwrap();
        assert_eq!(r.first_position, 3);
        q.advance(false);
        assert_eq!(cur(&q), "t1");
        assert_eq!(titles(&q.history(5)), vec!["t0"]);
        q.advance(false);
        q.advance(false);
        assert!(q.advance(false).is_none(), "queue over");
        assert!(!q.is_active());
        assert!(q.is_empty());
        // Adding after the end starts from the new track.
        let r = q.push(entries(1), 10).unwrap();
        assert_eq!(r.first_position, 0);
        assert_eq!(q.start().unwrap().info.youtube_id.as_deref(), Some("t0"));
        assert_eq!(q.history(10).len(), 4);
    }

    fn cur_after(q: &mut Queue) -> String {
        q.start().unwrap().info.youtube_id.clone().unwrap()
    }

    #[test]
    fn limit_counts_upcoming_only() {
        let mut q = Queue::new();
        q.push(entries(3), 3).unwrap();
        q.start();
        // current + 2 upcoming: one more fits.
        let r = q.push(entries(5), 3).unwrap();
        assert_eq!(
            r,
            PushResult {
                added: 1,
                first_position: 3,
                truncated: true
            }
        );
        assert_eq!(q.push(entries(1), 3), Err(PushError::Full { max: 3 }));
    }

    #[test]
    fn loop_modes() {
        let mut q = Queue::new();
        q.push(entries(2), 10).unwrap();
        q.start();
        q.loop_mode = LoopMode::Track;
        assert_eq!(q.advance(false).unwrap().info.youtube_id.as_deref(), Some("t0"));
        assert!(q.peek_next().is_none());
        // A skip leaves the looped track.
        assert_eq!(q.advance(true).unwrap().info.youtube_id.as_deref(), Some("t1"));
        q.loop_mode = LoopMode::Queue;
        assert_eq!(q.peek_next().unwrap().info.youtube_id.as_deref(), Some("t0"));
        assert_eq!(q.advance(false).unwrap().info.youtube_id.as_deref(), Some("t0"));
        assert_eq!(q.advance(false).unwrap().info.youtube_id.as_deref(), Some("t1"));
    }

    #[test]
    fn remove_move_skipto() {
        let mut q = Queue::new();
        q.push(entries(6), 10).unwrap();
        q.start(); // current t0, upcoming t1..t5
        assert!(q.remove(0).is_none());
        assert!(q.remove(6).is_none());
        assert_eq!(q.remove(2).unwrap().info.youtube_id.as_deref(), Some("t2"));
        assert_eq!(titles(q.upcoming()), vec!["t1", "t3", "t4", "t5"]);

        let (moved, to) = q.move_track(4, 1).unwrap();
        assert_eq!((moved.info.youtube_id.as_deref(), to), (Some("t5"), 1));
        assert_eq!(titles(q.upcoming()), vec!["t5", "t1", "t3", "t4"]);
        let (_, to) = q.move_track(1, 99).unwrap();
        assert_eq!(to, 4);
        assert_eq!(titles(q.upcoming()), vec!["t1", "t3", "t4", "t5"]);

        let uid = q.upcoming()[1].uid;
        assert_eq!(q.position_of(uid), Some(2));
        assert!(
            q.remove_uid(q.current().unwrap().uid).is_none(),
            "current is not removable"
        );

        let (dropped, next) = q.skip_to(3).unwrap();
        assert_eq!(titles(&dropped), vec!["t1", "t3"]);
        assert_eq!(next.info.youtube_id.as_deref(), Some("t4"));
        assert_eq!(titles(q.upcoming()), vec!["t5"]);
        // The skipped ones are gone, not in history.
        assert_eq!(titles(&q.history(10)), vec!["t0"]);
        assert!(q.skip_to(2).is_none());
    }

    #[test]
    fn shuffle_reorders_upcoming_only() {
        let mut q = Queue::new();
        q.push(entries(30), 100).unwrap();
        q.start();
        let before = titles(q.upcoming());
        let n = q.shuffle(&mut fastrand::Rng::with_seed(7));
        assert_eq!(n, 29);
        let after = titles(q.upcoming());
        assert_ne!(before, after);
        let mut sorted = after.clone();
        sorted.sort();
        let mut b = before.clone();
        b.sort();
        assert_eq!(sorted, b, "same tracks");
        assert_eq!(cur(&q), "t0");
    }

    #[test]
    fn previous_clear_and_restore() {
        let mut q = Queue::new();
        q.push(entries(3), 10).unwrap();
        assert!(q.previous().is_none());
        q.start();
        q.advance(false);
        assert_eq!(q.previous().unwrap().info.youtube_id.as_deref(), Some("t0"));
        assert_eq!(titles(&q.clear_upcoming()), vec!["t1", "t2"]);
        assert!(q.upcoming().is_empty());

        let r = Queue::restore(entries(4), 2, LoopMode::Queue);
        assert!(r.current().is_none());
        assert_eq!(r.cursor().unwrap().info.youtube_id.as_deref(), Some("t2"));
        assert_eq!(titles(r.upcoming()), vec!["t2", "t3"]);
        let r = Queue::restore(entries(2), 9, LoopMode::Off);
        assert!(r.cursor().is_none());

        // Adding to a restored queue that has not started keeps its upcoming tracks.
        let mut r = Queue::restore(entries(4), 2, LoopMode::Off);
        let res = r.push(entries(1), 10).unwrap();
        assert_eq!(res.first_position, 2, "after t3 (the cursor t2 counts as current)");
        assert_eq!(r.upcoming().len(), 3);
        assert_eq!(r.start().unwrap().info.youtube_id.as_deref(), Some("t2"));
    }

    #[test]
    fn played_tracks_are_trimmed() {
        let mut q = Queue::new();
        q.push(entries(250), 1000).unwrap();
        q.start();
        for _ in 0..200 {
            q.advance(false);
        }
        assert_eq!(q.index(), MAX_PLAYED_KEPT);
        assert_eq!(cur(&q), "t200");
        assert_eq!(q.all().len(), MAX_PLAYED_KEPT + 50);
    }

    #[test]
    fn session_roundtrip() {
        let e = QueueEntry::new(
            track("x"),
            Requester {
                id: 5,
                name: Some("n".into()),
                avatar_url: None,
            },
        );
        let back = QueueEntry::from_session(&e.to_session());
        assert_eq!(back.uid, e.uid);
        assert_eq!(back.requester.id, 5);
        assert!(back.source.is_none(), "single tracks have no source");

        let list = Arc::new(crate::sources::ListOrigin {
            name: "Mix".into(),
            image: None,
            platform: crate::sources::SourcePlatform::Spotify,
            url: "https://open.spotify.com/playlist/x".into(),
            total: 64,
        });
        let e = e.with_source(Some(TrackSource {
            list: list.clone(),
            index: 18,
        }));
        let back = QueueEntry::from_session(&e.to_session());
        let src = back.source.expect("source survives the session");
        assert_eq!((src.index, src.list.total), (18, 64));
        assert!(Arc::ptr_eq(&src.list, &list), "no copy in memory");
    }
}
