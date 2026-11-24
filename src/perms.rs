//! The single permission rule used by slash commands, buttons and (later) the panel API.
//!
//! - Manager: guild owner, Administrator or Manage Server.
//! - `control_mode = everyone`: anyone in the player's voice channel controls it.
//! - `control_mode = dj`: only DJs and managers control; others may add tracks and remove their own.
//! - "Play next" (add right after the current track) adds and reorders: it needs the Add rule and,
//!   in DJ mode, the Control rule too (a listener can't jump ahead of everyone else's songs).
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
    /// Add tracks right after the current one (`/play next:true`, the panel's "Add next"): in the
    /// channel, and allowed to control (DJ mode: DJs and managers only).
    AddNext,
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
        Action::AddNext if !actor.in_channel => Decision::Deny(Deny::NotInChannel),
        Action::AddNext => control(),
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

/// Jam actions (panel API and the "Invite to jam" button).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JamAction {
    /// Start a jam from the channel (or share the one it is in).
    Create,
    /// Make the user's channel a member. `replaces_queue`: its player has a queue, which is saved
    /// and replaced by the shared one while the jam lasts.
    Join { replaces_queue: bool },
    /// Take the user's channel out of the jam (its own queue comes back).
    Leave,
    /// Host settings, kick, close, end: only from the host channel.
    Manage,
}

/// The jam rule. `actor.in_channel` is about the channel the action concerns (the user's own for
/// create / join / leave, the host channel for manage), and nobody acts on a jam from outside it,
/// managers included:
/// - create, or join with an empty channel: anyone in the channel (like adding a song);
/// - join replacing a queue, leave, manage: the channel's normal control rule (DJ mode applies).
///
/// Editing the shared queue is the normal rule of the member's own server plus the jam's
/// `editable_by` setting (see `player::jam::Jams::edit_allowed`).
pub fn can_jam(actor: &Actor, action: JamAction, settings: &GuildSettings) -> Decision {
    if !actor.in_channel {
        return Decision::Deny(Deny::NotInChannel);
    }
    match action {
        JamAction::Create | JamAction::Join { replaces_queue: false } => Decision::Allow,
        JamAction::Join { replaces_queue: true } | JamAction::Leave | JamAction::Manage => {
            can(actor, Action::Control, settings)
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
    fn add_next_needs_control_in_dj_mode() {
        let everyone = settings(ControlMode::Everyone, false);
        let dj = settings(ControlMode::Dj, false);
        assert_eq!(can(&LISTENER, Action::AddNext, &everyone), Decision::Allow);
        assert_eq!(can(&LISTENER, Action::AddNext, &dj), Decision::Deny(Deny::DjOnly));
        assert_eq!(can(&LISTENER, Action::Add, &dj), Decision::Allow, "adding at the end stays open");
        let a_dj = Actor {
            is_dj: true,
            ..LISTENER
        };
        assert_eq!(can(&a_dj, Action::AddNext, &dj), Decision::Allow);
        // Like adding, it needs the user in the channel, managers included.
        let manager_outside = Actor {
            is_manager: true,
            in_channel: false,
            ..LISTENER
        };
        assert_eq!(
            can(&manager_outside, Action::AddNext, &dj),
            Decision::Deny(Deny::NotInChannel)
        );
        let manager = Actor {
            is_manager: true,
            ..LISTENER
        };
        assert_eq!(can(&manager, Action::AddNext, &dj), Decision::Allow);
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
    fn jam_rules() {
        let dj_mode = settings(ControlMode::Dj, false);
        let everyone = settings(ControlMode::Everyone, false);
        let outside_manager = Actor {
            is_manager: true,
            in_channel: false,
            ..LISTENER
        };
        for action in [
            JamAction::Create,
            JamAction::Join { replaces_queue: false },
            JamAction::Leave,
            JamAction::Manage,
        ] {
            assert_eq!(
                can_jam(&outside_manager, action, &everyone),
                Decision::Deny(Deny::NotInChannel),
                "{action:?} needs the channel, even for managers"
            );
        }
        assert_eq!(can_jam(&LISTENER, JamAction::Create, &dj_mode), Decision::Allow);
        assert_eq!(
            can_jam(&LISTENER, JamAction::Join { replaces_queue: false }, &dj_mode),
            Decision::Allow
        );
        for action in [JamAction::Join { replaces_queue: true }, JamAction::Leave, JamAction::Manage] {
            assert_eq!(can_jam(&LISTENER, action, &everyone), Decision::Allow);
            assert_eq!(can_jam(&LISTENER, action, &dj_mode), Decision::Deny(Deny::DjOnly));
            let dj = Actor {
                is_dj: true,
                ..LISTENER
            };
            assert_eq!(can_jam(&dj, action, &dj_mode), Decision::Allow);
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
