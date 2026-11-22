//! Voice channel lifecycle rules that do not need Discord: the empty-channel timer.
//!
//! When the last human leaves, playback pauses and a deadline starts (`EMPTY_CHANNEL_TIMEOUT_SECS`).
//! Someone joining before it resumes (unless a user had paused it); reaching it deletes the queue.

use std::time::Duration;

use tokio::time::Instant;

/// How long the bot stays after the queue ended before leaving.
pub const QUEUE_END_LEAVE_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyAction {
    Nothing,
    /// Pause and warn; the queue is deleted at `deadline`.
    Pause {
        deadline: Instant,
    },
    /// Someone came back. `play` is false when the music was already paused by a user.
    Resume {
        play: bool,
    },
}

#[derive(Debug, Clone, Default)]
pub struct EmptyChannel {
    deadline: Option<Instant>,
    resume_after: bool,
}

impl EmptyChannel {
    /// Feed the number of humans now in the channel. `playing` is whether music is currently
    /// playing (not paused by a user).
    pub fn update(&mut self, humans: usize, playing: bool, now: Instant, timeout: Duration) -> EmptyAction {
        match (humans, self.deadline) {
            (0, None) => {
                let deadline = now + timeout;
                self.deadline = Some(deadline);
                self.resume_after = playing;
                EmptyAction::Pause { deadline }
            }
            (n, Some(_)) if n > 0 => {
                self.deadline = None;
                EmptyAction::Resume {
                    play: self.resume_after,
                }
            }
            _ => EmptyAction::Nothing,
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn is_waiting(&self) -> bool {
        self.deadline.is_some()
    }

    /// A user paused while the channel is empty: someone joining later must not start the music.
    pub fn user_paused(&mut self) {
        self.resume_after = false;
    }

    /// A user resumed while the channel is empty (panel): keep playing when people come back.
    pub fn user_resumed(&mut self) {
        self.resume_after = true;
    }
}

/// Convert a tokio deadline to epoch milliseconds (API) using the current wall clock.
pub fn deadline_epoch_ms(deadline: Instant) -> i64 {
    let now_ms = crate::player::state::epoch_ms();
    let left = deadline.saturating_duration_since(Instant::now());
    now_ms + left.as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(180);

    #[test]
    fn transitions() {
        let now = Instant::now();
        let mut e = EmptyChannel::default();
        assert_eq!(e.update(2, true, now, T), EmptyAction::Nothing);
        assert_eq!(
            e.update(1, true, now, T),
            EmptyAction::Nothing,
            "others leaving keeps playing"
        );
        assert_eq!(e.update(0, true, now, T), EmptyAction::Pause { deadline: now + T });
        assert_eq!(e.update(0, false, now, T), EmptyAction::Nothing, "already waiting");
        assert_eq!(e.update(1, false, now, T), EmptyAction::Resume { play: true });
        assert!(!e.is_waiting());

        // Paused by a user before everyone left: rejoining does not start the music.
        assert_eq!(e.update(0, false, now, T), EmptyAction::Pause { deadline: now + T });
        assert_eq!(e.update(3, false, now, T), EmptyAction::Resume { play: false });
    }

    /// The deadline drives a `sleep_until` in the player loop; check it with paused time.
    #[tokio::test(start_paused = true)]
    async fn timer_fires_after_timeout_unless_someone_joins() {
        let mut e = EmptyChannel::default();
        let EmptyAction::Pause { deadline } = e.update(0, true, Instant::now(), T) else {
            panic!()
        };

        // Nobody joins within 179 s: still waiting.
        let fired = tokio::time::timeout(Duration::from_secs(179), tokio::time::sleep_until(deadline)).await;
        assert!(fired.is_err());
        // At 180 s the timer fires.
        tokio::time::sleep_until(deadline).await;
        assert!(Instant::now() >= deadline);

        // Second run: someone joins after 60 s, the deadline is cleared.
        let mut e = EmptyChannel::default();
        e.update(0, true, Instant::now(), T);
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(e.update(1, true, Instant::now(), T), EmptyAction::Resume { play: true });
        assert!(e.deadline().is_none());
        let left = deadline_epoch_ms(Instant::now() + Duration::from_secs(10)) - crate::player::state::epoch_ms();
        assert!((9_000..=10_000).contains(&left), "{left}");
    }
}
