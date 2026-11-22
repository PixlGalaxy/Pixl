//! Button custom ids. Every id names the voice channel (player) it acts on, so a button keeps
//! working when several players share a text channel, and outdated buttons can be detected.

/// How long the queue pagination buttons stay usable.
pub const QUEUE_BUTTONS_TTL_SECS: i64 = 15 * 60;

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
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomId {
    /// Now playing buttons. `track` is the short tag of the queue entry the message was made for.
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
}

impl CustomId {
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
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split(':');
        match parts.next()? {
            "np" => {
                let action = NpAction::parse(parts.next()?)?;
                let voice_channel_id = parts.next()?.parse().ok()?;
                let track = parts.next()?.to_owned();
                Some(Self::NowPlaying {
                    action,
                    voice_channel_id,
                    track,
                })
            }
            "q" => {
                let voice_channel_id = parts.next()?.parse().ok()?;
                let page = parts.next()?.parse().ok()?;
                let issued = parts.next()?.parse().ok()?;
                Some(Self::QueuePage {
                    voice_channel_id,
                    page,
                    issued,
                })
            }
            _ => None,
        }
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
                action: NpAction::Dislike,
                voice_channel_id: u64::MAX,
                track: "x".into(),
            },
            CustomId::QueuePage {
                voice_channel_id: 5,
                page: 3,
                issued: 1_700_000_000,
            },
        ] {
            let s = id.encode();
            assert!(s.len() <= 100);
            assert_eq!(CustomId::parse(&s), Some(id));
        }
        assert_eq!(CustomId::parse("np:nope:1:a"), None);
        assert_eq!(CustomId::parse("garbage"), None);
        assert_eq!(track_tag(&uuid::Uuid::nil()).len(), 12);
    }
}
