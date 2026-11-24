//! Button custom ids. Every id names the voice channel (player) it acts on, so a button keeps
//! working when several players share a text channel, and outdated buttons can be detected.

/// How long the queue pagination buttons stay usable.
pub const QUEUE_BUTTONS_TTL_SECS: i64 = 5 * 60;

/// Track tag used by buttons that are not tied to one track (Open panel on a queue card...).
pub const NO_TRACK: &str = "-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpAction {
    Pause,
    Resume,
    Skip,
    Stop,
    Queue,
    Like,
    Dislike,
    Panel,
    Jam,
}

impl NpAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Skip => "skip",
            Self::Stop => "stop",
            Self::Queue => "queue",
            Self::Like => "like",
            Self::Dislike => "dislike",
            Self::Panel => "panel",
            Self::Jam => "jam",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pause" => Self::Pause,
            "resume" => Self::Resume,
            "skip" => Self::Skip,
            "stop" => Self::Stop,
            "queue" => Self::Queue,
            "like" => Self::Like,
            "dislike" => Self::Dislike,
            "panel" => Self::Panel,
            "jam" => Self::Jam,
            _ => return None,
        })
    }
}

/// How long the `/playlist` picker buttons stay usable.
pub const PICKER_TTL_SECS: i64 = 5 * 60;

/// The components of the `/playlist` picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickAction {
    Previous,
    Next,
    Select,
    /// The "Jump to a playlist" menu (the chosen id comes in the interaction's values).
    Jump,
}

impl PickAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Previous => "p",
            Self::Next => "n",
            Self::Select => "s",
            Self::Jump => "j",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "p" => Self::Previous,
            "n" => Self::Next,
            "s" => Self::Select,
            "j" => Self::Jump,
            _ => return None,
        })
    }
}

/// Which saved playlists the picker pages through (re-read from the clicking user's lists on
/// every click, so the custom id never carries a list of ids).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickScope {
    /// Every saved playlist of the user.
    All,
    /// The user's saved playlists with the same name as the shown one.
    SameName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomId {
    /// Player buttons. `track` is the short tag of the entry the message was made for (or `NO_TRACK`).
    NowPlaying {
        action: NpAction,
        voice_channel_id: u64,
        track: String,
    },
    /// Queue pagination; `issued` is the unix time the message was created (buttons expire).
    QueuePage {
        voice_channel_id: u64,
        page: u32,
        issued: i64,
    },
    /// "Play it again" on the queue-deleted notice.
    Replay { voice_channel_id: u64 },
    /// "Start radio" on the queue-finished notice.
    StartRadio { voice_channel_id: u64 },
    /// "Stop radio" on the "Radio is on" card.
    StopRadio { voice_channel_id: u64 },
    /// "Cancel" on the "Radio is picking songs" card; `job` is the job uuid (simple form).
    CancelRadio { voice_channel_id: u64, job: String },
    /// `/playlist` picker: `shown` is the saved playlist id on screen, `user_id` ran the command
    /// at `issued` (unix seconds).
    PlaylistPick {
        action: PickAction,
        shown: i64,
        scope: PickScope,
        user_id: u64,
        issued: i64,
    },
}

impl CustomId {
    pub fn np(action: NpAction, voice_channel_id: u64, track: &str) -> String {
        Self::NowPlaying {
            action,
            voice_channel_id,
            track: track.to_owned(),
        }
        .encode()
    }

    pub fn encode(&self) -> String {
        match self {
            Self::NowPlaying {
                action,
                voice_channel_id,
                track,
            } => {
                format!("np:{}:{voice_channel_id}:{track}", action.as_str())
            }
            Self::QueuePage {
                voice_channel_id,
                page,
                issued,
            } => format!("q:{voice_channel_id}:{page}:{issued}"),
            Self::Replay { voice_channel_id } => format!("re:{voice_channel_id}"),
            Self::StartRadio { voice_channel_id } => format!("ai:{voice_channel_id}"),
            Self::StopRadio { voice_channel_id } => format!("rs:{voice_channel_id}"),
            Self::CancelRadio { voice_channel_id, job } => format!("rc:{voice_channel_id}:{job}"),
            Self::PlaylistPick {
                action,
                shown,
                scope,
                user_id,
                issued,
            } => {
                let scope = match scope {
                    PickScope::All => "a",
                    PickScope::SameName => "n",
                };
                format!("pl:{}:{shown}:{scope}:{user_id}:{issued}", action.as_str())
            }
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split(':');
        let id = match parts.next()? {
            "np" => {
                let action = NpAction::parse(parts.next()?)?;
                let voice_channel_id = parts.next()?.parse().ok()?;
                let track = parts.next()?.to_owned();
                Self::NowPlaying {
                    action,
                    voice_channel_id,
                    track,
                }
            }
            "q" => {
                let voice_channel_id = parts.next()?.parse().ok()?;
                let page = parts.next()?.parse().ok()?;
                let issued = parts.next()?.parse().ok()?;
                Self::QueuePage {
                    voice_channel_id,
                    page,
                    issued,
                }
            }
            "re" => Self::Replay {
                voice_channel_id: parts.next()?.parse().ok()?,
            },
            "ai" => Self::StartRadio {
                voice_channel_id: parts.next()?.parse().ok()?,
            },
            "rs" => Self::StopRadio {
                voice_channel_id: parts.next()?.parse().ok()?,
            },
            "rc" => Self::CancelRadio {
                voice_channel_id: parts.next()?.parse().ok()?,
                job: parts.next()?.to_owned(),
            },
            "pl" => Self::PlaylistPick {
                action: PickAction::parse(parts.next()?)?,
                shown: parts.next()?.parse().ok()?,
                scope: match parts.next()? {
                    "a" => PickScope::All,
                    "n" => PickScope::SameName,
                    _ => return None,
                },
                user_id: parts.next()?.parse().ok()?,
                issued: parts.next()?.parse().ok()?,
            },
            _ => return None,
        };
        parts.next().is_none().then_some(id)
    }
}

/// Short tag of a queue entry uid, used in now-playing button ids.
pub fn track_tag(uid: &uuid::Uuid) -> String {
    uid.simple().to_string()[..12].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for id in [
            CustomId::NowPlaying {
                action: NpAction::Skip,
                voice_channel_id: 123,
                track: "abcdef".into(),
            },
            CustomId::NowPlaying {
                action: NpAction::Jam,
                voice_channel_id: u64::MAX,
                track: NO_TRACK.into(),
            },
            CustomId::QueuePage {
                voice_channel_id: 5,
                page: 3,
                issued: 1_700_000_000,
            },
            CustomId::Replay { voice_channel_id: 9 },
            CustomId::StartRadio { voice_channel_id: 9 },
            CustomId::StopRadio { voice_channel_id: 9 },
            CustomId::CancelRadio {
                voice_channel_id: u64::MAX,
                job: uuid::Uuid::new_v4().simple().to_string(),
            },
            CustomId::PlaylistPick {
                action: PickAction::Next,
                shown: 17,
                scope: PickScope::SameName,
                user_id: 123_456_789_012_345_678,
                issued: 1_700_000_000,
            },
            CustomId::PlaylistPick {
                action: PickAction::Jump,
                shown: i64::MAX,
                scope: PickScope::All,
                user_id: u64::MAX,
                issued: i64::MAX,
            },
        ] {
            let s = id.encode();
            assert!(s.len() <= 100);
            assert_eq!(CustomId::parse(&s), Some(id));
        }
        assert_eq!(CustomId::parse("np:nope:1:a"), None);
        assert_eq!(CustomId::parse("re:1:extra"), None);
        assert_eq!(CustomId::parse("garbage"), None);
        assert_eq!(CustomId::parse("pl:x:1:a:1:2"), None, "unknown picker action");
        assert_eq!(CustomId::parse("pl:s:1:z:1:2"), None, "unknown scope");
        assert_eq!(CustomId::parse("pl:s:1:a:1:2:3"), None, "extra part");
        assert_eq!(track_tag(&uuid::Uuid::nil()).len(), 12);
    }
}
