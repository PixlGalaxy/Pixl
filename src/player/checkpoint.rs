//! Time checkpoints: the playing position is saved with the session, and a restored session goes on
//! from there (a little before, so nothing is missed).

use std::time::Duration;

use crate::store::LoopMode;

/// How often a playing track's position is saved (through the debounced session writer).
pub const CHECKPOINT_EVERY: Duration = Duration::from_secs(5);
/// A resumed track starts this much before the saved position.
pub const RESUME_OVERLAP_MS: u64 = 2_000;
/// A saved position this close to the end (or past it) plays the next track instead.
pub const NEAR_END_MS: u64 = 5_000;

/// What to do with the saved current track of a restored session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumePlan {
    /// Play it from the start.
    Start,
    /// Play it and seek here once it is playable.
    SeekTo(u64),
    /// It was practically over: start the next track.
    Next,
}

/// Decide how a restored track resumes. `duration_ms` is None (or 0) for live or unknown lengths:
/// then the seek is tried and a failure plays it from the start.
pub fn resume_plan(position_ms: u64, duration_ms: Option<u64>) -> ResumePlan {
    if position_ms == 0 {
        return ResumePlan::Start;
    }
    if let Some(d) = duration_ms.filter(|d| *d > 0) {
        if position_ms.saturating_add(NEAR_END_MS) >= d {
            return ResumePlan::Next;
        }
    }
    match position_ms.saturating_sub(RESUME_OVERLAP_MS) {
        0 => ResumePlan::Start,
        target => ResumePlan::SeekTo(target),
    }
}

/// Index of the track that follows `index` when it ends normally (restore of a finished track).
/// `len` means the queue is over.
pub fn next_index(index: usize, len: usize, loop_mode: LoopMode) -> usize {
    match loop_mode {
        LoopMode::Track => index,
        LoopMode::Queue if len > 0 => (index + 1) % len,
        _ => (index + 1).min(len),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_target_keeps_an_overlap() {
        assert_eq!(resume_plan(90_000, Some(200_000)), ResumePlan::SeekTo(88_000));
        assert_eq!(resume_plan(2_500, Some(200_000)), ResumePlan::SeekTo(500));
        // Within the overlap of the start: just start.
        assert_eq!(resume_plan(1_500, Some(200_000)), ResumePlan::Start);
        assert_eq!(resume_plan(2_000, Some(200_000)), ResumePlan::Start);
        assert_eq!(resume_plan(0, Some(200_000)), ResumePlan::Start);
    }

    #[test]
    fn near_end_plays_the_next_track() {
        assert_eq!(resume_plan(195_000, Some(200_000)), ResumePlan::Next);
        assert_eq!(resume_plan(199_999, Some(200_000)), ResumePlan::Next);
        assert_eq!(resume_plan(250_000, Some(200_000)), ResumePlan::Next);
        assert_eq!(resume_plan(194_999, Some(200_000)), ResumePlan::SeekTo(192_999));
        // Very short track, saved at 0: still from the start.
        assert_eq!(resume_plan(0, Some(3_000)), ResumePlan::Start);
    }

    #[test]
    fn missing_duration_tries_the_seek() {
        assert_eq!(resume_plan(60_000, None), ResumePlan::SeekTo(58_000));
        assert_eq!(resume_plan(60_000, Some(0)), ResumePlan::SeekTo(58_000));
        assert_eq!(resume_plan(1_000, None), ResumePlan::Start);
    }

    #[test]
    fn next_index_follows_the_loop_mode() {
        assert_eq!(next_index(2, 5, LoopMode::Off), 3);
        assert_eq!(next_index(4, 5, LoopMode::Off), 5, "queue over");
        assert_eq!(next_index(4, 5, LoopMode::Queue), 0);
        assert_eq!(next_index(1, 5, LoopMode::Queue), 2);
        assert_eq!(
            next_index(3, 5, LoopMode::Track),
            3,
            "the loop replays it from the start"
        );
    }
}
