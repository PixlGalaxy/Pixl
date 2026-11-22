//! The single permission rule used by slash commands, buttons and (later) the panel API.
//!
//! - Manager: guild owner, Administrator or Manage Server.
//! - `control_mode = everyone`: anyone in the player's voice channel controls it.
//! - `control_mode = dj`: only DJs and managers control; others may add tracks and remove their own.
//! - Vote skip (when enabled): DJs, managers and the requester of the current track skip directly;
//!   everyone else votes.

use serde::Serialize;

use crate::store::{ControlMode, GuildSettings};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// See the queue, now playing, lyrics.
    View,
    /// Add tracks (`/play`, panel search).
    Add,
    /// Pause, resume, stop, seek, volume, loop, shuffle, move, skipto, clear, previous.
    Control,
    Skip,
    /// Remove one upcoming track; `own` when the user requested it.
    Remove {
        own: bool,
    },
    /// Like / Dislike the current track.
    Feedback,
    /// Change guild settings and DJs (panel API).
    #[allow(dead_code)]
    ManageSettings,
}

/// Who is asking, relative to one player.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Actor {
    pub is_manager: bool,
    pub is_dj: bool,
    /// The user is in the player's voice channel.
    pub in_channel: bool,
    /// The user requested the track that is playing now.
    pub requested_current: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deny {
    NotInChannel,
    DjOnly,
    ManagerOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Skip only: the request counts as a vote.
    Vote,
    Deny(Deny),
}

impl Decision {
    #[allow(dead_code)]
    pub fn allowed(self) -> bool {
        matches!(self, Self::Allow)
    }
}

pub fn can(actor: &Actor, action: Action, settings: &GuildSettings) -> Decision {
    let privileged = actor.is_manager || actor.is_dj;
    let present = actor.in_channel || actor.is_manager;
    let control = || {
        if !present {
            Decision::Deny(Deny::NotInChannel)
        } else if settings.control_mode == ControlMode::Everyone || privileged {
            Decision::Allow
        } else {
            Decision::Deny(Deny::DjOnly)
        }
    };
    match action {
        Action::View => Decision::Allow,
        Action::ManageSettings if actor.is_manager => Decision::Allow,
        Action::ManageSettings => Decision::Deny(Deny::ManagerOnly),
        Action::Add | Action::Feedback if actor.in_channel => Decision::Allow,
        Action::Add | Action::Feedback => Decision::Deny(Deny::NotInChannel),
        Action::Control => control(),
        Action::Remove { own: true } if actor.in_channel => Decision::Allow,
        Action::Remove { .. } => control(),
        Action::Skip => {
            if !present {
                Decision::Deny(Deny::NotInChannel)
            } else if privileged || actor.requested_current {
                Decision::Allow
            } else if settings.vote_skip.enabled {
                Decision::Vote
            } else if settings.control_mode == ControlMode::Everyone {
                Decision::Allow
            } else {
                Decision::Deny(Deny::DjOnly)
            }
        }
    }
}

/// Channel restriction rule: an empty list allows every channel; managers are never restricted.
pub fn channel_allowed(allowed: &[u64], channel: u64, is_manager: bool) -> bool {
    is_manager || allowed.is_empty() || allowed.contains(&channel)
}

/// API `Permissions` object (panel API phase).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Permissions {
    pub is_manager: bool,
    pub is_dj: bool,
    pub can_control: bool,
    pub can_add: bool,
}

#[allow(dead_code)]
impl Permissions {
    pub fn of(actor: &Actor, settings: &GuildSettings) -> Self {
        Self {
            is_manager: actor.is_manager,
            is_dj: actor.is_dj,
            can_control: can(actor, Action::Control, settings).allowed(),
            can_add: can(actor, Action::Add, settings).allowed(),
        }
    }
}

/// Discord permission bits that make someone a Lynx manager.
pub const ADMINISTRATOR: u64 = 1 << 3;
pub const MANAGE_GUILD: u64 = 1 << 5;

pub fn is_manager_bits(permissions: u64, is_owner: bool) -> bool {
    is_owner || permissions & (ADMINISTRATOR | MANAGE_GUILD) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{VoteSkip, VoteSkipMode};

    fn settings(mode: ControlMode, vote: bool) -> GuildSettings {
        GuildSettings {
            control_mode: mode,
            vote_skip: VoteSkip {
                enabled: vote,
                mode: VoteSkipMode::Percent,
                value: 50,
            },
            ..GuildSettings::default()
        }
    }

    const LISTENER: Actor = Actor {
        is_manager: false,
        is_dj: false,
        in_channel: true,
        requested_current: false,
    };

    #[test]
    fn everyone_mode() {
        let s = settings(ControlMode::Everyone, false);
        assert_eq!(can(&LISTENER, Action::Control, &s), Decision::Allow);
        assert_eq!(can(&LISTENER, Action::Skip, &s), Decision::Allow);
        assert_eq!(can(&LISTENER, Action::Remove { own: false }, &s), Decision::Allow);
        assert_eq!(
            can(&LISTENER, Action::ManageSettings, &s),
            Decision::Deny(Deny::ManagerOnly)
        );
        let outside = Actor {
            in_channel: false,
            ..LISTENER
        };
        assert_eq!(can(&outside, Action::Control, &s), Decision::Deny(Deny::NotInChannel));
        assert_eq!(can(&outside, Action::Add, &s), Decision::Deny(Deny::NotInChannel));
        assert_eq!(can(&outside, Action::View, &s), Decision::Allow);
    }

    #[test]
    fn dj_mode() {
        let s = settings(ControlMode::Dj, false);
        assert_eq!(can(&LISTENER, Action::Control, &s), Decision::Deny(Deny::DjOnly));
        assert_eq!(can(&LISTENER, Action::Add, &s), Decision::Allow);
        assert_eq!(can(&LISTENER, Action::Remove { own: true }, &s), Decision::Allow);
        assert_eq!(
            can(&LISTENER, Action::Remove { own: false }, &s),
            Decision::Deny(Deny::DjOnly)
        );
        assert_eq!(can(&LISTENER, Action::Skip, &s), Decision::Deny(Deny::DjOnly));
        let requester = Actor {
            requested_current: true,
            ..LISTENER
        };
        assert_eq!(can(&requester, Action::Skip, &s), Decision::Allow);
        let dj = Actor {
            is_dj: true,
            ..LISTENER
        };
        assert_eq!(can(&dj, Action::Control, &s), Decision::Allow);
        // Managers can act from outside the channel (panel).
        let manager = Actor {
            is_manager: true,
            in_channel: false,
            ..LISTENER
        };
        assert_eq!(can(&manager, Action::Control, &s), Decision::Allow);
        assert_eq!(can(&manager, Action::ManageSettings, &s), Decision::Allow);
    }

    #[test]
    fn vote_skip() {
        for mode in [ControlMode::Everyone, ControlMode::Dj] {
            let s = settings(mode, true);
            assert_eq!(can(&LISTENER, Action::Skip, &s), Decision::Vote);
            assert_eq!(
                can(
                    &Actor {
                        is_dj: true,
                        ..LISTENER
                    },
                    Action::Skip,
                    &s
                ),
                Decision::Allow
            );
            assert_eq!(
                can(
                    &Actor {
                        is_manager: true,
                        ..LISTENER
                    },
                    Action::Skip,
                    &s
                ),
                Decision::Allow
            );
            assert_eq!(
                can(
                    &Actor {
                        requested_current: true,
                        ..LISTENER
                    },
                    Action::Skip,
                    &s
                ),
                Decision::Allow
            );
        }
    }

    #[test]
    fn manager_bits_and_api_shape() {
        assert!(is_manager_bits(ADMINISTRATOR, false));
        assert!(is_manager_bits(MANAGE_GUILD, false));
        assert!(is_manager_bits(0, true));
        assert!(!is_manager_bits(1 << 11, false));
        let p = Permissions::of(&LISTENER, &settings(ControlMode::Dj, false));
        let json = serde_json::to_value(p).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"isManager": false, "isDj": false, "canControl": false, "canAdd": true})
        );
    }
}
