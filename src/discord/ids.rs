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
    /// "Start radio" on the queue-finished notice (AI phase).
    StartRadio { voice_channel_id: u64 },
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
        ] {
            let s = id.encode();
            assert!(s.len() <= 100);
            assert_eq!(CustomId::parse(&s), Some(id));
        }
        assert_eq!(CustomId::parse("np:nope:1:a"), None);
        assert_eq!(CustomId::parse("re:1:extra"), None);
        assert_eq!(CustomId::parse("garbage"), None);
        assert_eq!(track_tag(&uuid::Uuid::nil()).len(), 12);
    }
}
