//! Vote skip: how many votes are needed and who already voted for the current track.

use std::collections::HashSet;

use crate::store::{VoteSkip, VoteSkipMode};

/// Votes needed to skip with `listeners` humans in the voice channel (always at least 1, never
/// more than the listeners present).
pub fn required_votes(rule: &VoteSkip, listeners: usize) -> usize {
    let listeners = listeners.max(1);
    let needed = match rule.mode {
        VoteSkipMode::Count => rule.value as usize,
        VoteSkipMode::Percent => (listeners * rule.value.clamp(1, 100) as usize).div_ceil(100),
    };
    needed.clamp(1, listeners)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteResult {
    /// Enough votes: skip now.
    Passed,
    Counted {
        current: usize,
        required: usize,
    },
    AlreadyVoted {
        current: usize,
        required: usize,
    },
}

/// Votes for the current track; reset whenever the track changes.
#[derive(Debug, Default, Clone)]
pub struct Votes {
    voters: HashSet<u64>,
}

impl Votes {
    pub fn reset(&mut self) {
        self.voters.clear();
    }

    /// Votes that still count (only people still in the channel).
    pub fn current(&self, listeners: &HashSet<u64>) -> usize {
        self.voters.iter().filter(|v| listeners.contains(v)).count()
    }

    pub fn vote(&mut self, user: u64, listeners: &HashSet<u64>, rule: &VoteSkip) -> VoteResult {
        self.voters.retain(|v| listeners.contains(v));
        let required = required_votes(rule, listeners.len());
        let fresh = self.voters.insert(user);
        let current = self.voters.len();
        if current >= required {
            VoteResult::Passed
        } else if fresh {
            VoteResult::Counted { current, required }
        } else {
            VoteResult::AlreadyVoted { current, required }
        }
    }

    /// Recheck after people left: fewer listeners can make the existing votes enough.
    pub fn passes(&self, listeners: &HashSet<u64>, rule: &VoteSkip) -> bool {
        let current = self.current(listeners);
        current > 0 && current >= required_votes(rule, listeners.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(mode: VoteSkipMode, value: u32) -> VoteSkip {
        VoteSkip {
            enabled: true,
            mode,
            value,
        }
    }

    #[test]
    fn required_math() {
        let pct = rule(VoteSkipMode::Percent, 50);
        assert_eq!(required_votes(&pct, 1), 1);
        assert_eq!(required_votes(&pct, 2), 1);
        assert_eq!(required_votes(&pct, 3), 2);
        assert_eq!(required_votes(&pct, 4), 2);
        assert_eq!(required_votes(&pct, 0), 1);
        assert_eq!(required_votes(&rule(VoteSkipMode::Percent, 100), 5), 5);
        assert_eq!(required_votes(&rule(VoteSkipMode::Percent, 1), 50), 1);
        let count = rule(VoteSkipMode::Count, 3);
        assert_eq!(required_votes(&count, 10), 3);
        // Never more than the people present.
        assert_eq!(required_votes(&count, 2), 2);
    }

    #[test]
    fn voting() {
        let r = rule(VoteSkipMode::Count, 3);
        let listeners: HashSet<u64> = [1, 2, 3, 4].into();
        let mut v = Votes::default();
        assert_eq!(
            v.vote(1, &listeners, &r),
            VoteResult::Counted {
                current: 1,
                required: 3
            }
        );
        assert_eq!(
            v.vote(1, &listeners, &r),
            VoteResult::AlreadyVoted {
                current: 1,
                required: 3
            }
        );
        assert_eq!(
            v.vote(2, &listeners, &r),
            VoteResult::Counted {
                current: 2,
                required: 3
            }
        );
        // Voter 2 leaves: their vote no longer counts.
        let fewer: HashSet<u64> = [1, 3, 4].into();
        assert_eq!(v.current(&fewer), 1);
        assert_eq!(
            v.vote(3, &fewer, &r),
            VoteResult::Counted {
                current: 2,
                required: 3
            }
        );
        assert_eq!(v.vote(4, &fewer, &r), VoteResult::Passed);
        v.reset();
        assert_eq!(v.current(&fewer), 0);

        // Two listeners left with two votes in count mode 3: passes on recheck.
        let mut v = Votes::default();
        v.vote(1, &listeners, &r);
        v.vote(3, &listeners, &r);
        let two: HashSet<u64> = [1, 3].into();
        assert!(v.passes(&two, &r));
        assert!(!Votes::default().passes(&two, &r));
    }
}
