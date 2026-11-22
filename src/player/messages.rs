//! The text messages of one player, always sent by the MAIN bot. A small task per player keeps one
//! "now playing" message and one status notice; each new one replaces (deletes) the previous.
//! Sending never blocks the player: the queue is bounded and overflow is dropped.

use std::{sync::Arc, time::Duration};

use serenity::{
    all::{ChannelId, MessageId},
    http::Http,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use super::queue::Entry;
use crate::{
    discord::{
        ids::track_tag,
        ui::{
            cards::{self, Notice, NowPlaying, TrackCard},
            Message,
        },
    },
    i18n::I18n,
    store::{track_key, Store},
};

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const QUEUE: usize = 32;

/// Owned version of `cards::Notice`.
#[derive(Debug, Clone)]
pub enum OwnedNotice {
    PausedEmpty {
        deadline_secs: i64,
    },
    Resumed {
        title: String,
        url: String,
    },
    QueueDeleted {
        minutes: u64,
    },
    BackOnline {
        title: String,
        url: String,
        position: usize,
        total: usize,
    },
    QueueFinished,
    TrackFailed {
        title: String,
        url: String,
        reason: String,
    },
    TooManyFailures,
    Disconnected,
    Stopped {
        by: u64,
    },
}

impl OwnedNotice {
    fn card(&self, ai_enabled: bool) -> Notice<'_> {
        match self {
            Self::PausedEmpty { deadline_secs } => Notice::PausedEmpty {
                deadline_secs: *deadline_secs,
            },
            Self::Resumed { title, url } => Notice::Resumed { title, url },
            Self::QueueDeleted { minutes } => Notice::QueueDeleted { minutes: *minutes },
            Self::BackOnline {
                title,
                url,
                position,
                total,
            } => Notice::BackOnline {
                title,
                url,
                position: *position,
                total: *total,
            },
            Self::QueueFinished => Notice::QueueFinished { ai_enabled },
            Self::TrackFailed { title, url, reason } => Notice::TrackFailed { title, url, reason },
            Self::TooManyFailures => Notice::TooManyFailures,
            Self::Disconnected => Notice::Disconnected,
            Self::Stopped { by } => Notice::Stopped { by: *by },
        }
    }
}

#[derive(Debug, Clone)]
pub struct NpData {
    pub entry: Entry,
    pub node_name: Option<String>,
    pub paused: bool,
}

enum Out {
    SetChannel(Option<u64>),
    NowPlaying(NpData),
    /// Edit the current now-playing message in place (pause state changed).
    RefreshNowPlaying(NpData),
    ClearNowPlaying,
    Notice(OwnedNotice),
    Close {
        delete_now_playing: bool,
        done: oneshot::Sender<()>,
    },
}

#[derive(Clone)]
pub struct Messenger {
    tx: mpsc::Sender<Out>,
}

pub struct MessengerDeps {
    pub http: Arc<Http>,
    pub i18n: Arc<I18n>,
    pub store: Store,
    pub ai_enabled: bool,
    pub voice_channel_id: u64,
}

impl Messenger {
    pub fn spawn(deps: MessengerDeps, text_channel: Option<u64>) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE);
        tokio::spawn(run(deps, text_channel, rx));
        Self { tx }
    }

    fn send(&self, out: Out) {
        if self.tx.try_send(out).is_err() {
            debug!("message queue full, dropping a player message");
        }
    }

    pub fn set_channel(&self, channel: Option<u64>) {
        self.send(Out::SetChannel(channel));
    }

    pub fn now_playing(&self, data: NpData) {
        self.send(Out::NowPlaying(data));
    }

    pub fn refresh_now_playing(&self, data: NpData) {
        self.send(Out::RefreshNowPlaying(data));
    }

    pub fn clear_now_playing(&self) {
        self.send(Out::ClearNowPlaying);
    }

    pub fn notice(&self, n: OwnedNotice) {
        self.send(Out::Notice(n));
    }

    /// Stop the task after the pending messages, optionally deleting the now-playing message.
    pub async fn close(&self, delete_now_playing: bool) {
        let (done, rx) = oneshot::channel();
        if tokio::time::timeout(
            Duration::from_secs(2),
            self.tx.send(Out::Close {
                delete_now_playing,
                done,
            }),
        )
        .await
        .is_ok()
        {
            let _ = tokio::time::timeout(Duration::from_secs(15), rx).await;
        }
    }
}

struct Slots {
    channel: Option<u64>,
    now_playing: Option<(u64, u64)>,
    status: Option<(u64, u64)>,
}

async fn run(deps: MessengerDeps, channel: Option<u64>, mut rx: mpsc::Receiver<Out>) {
    let mut s = Slots {
        channel,
        now_playing: None,
        status: None,
    };
    while let Some(out) = rx.recv().await {
        match out {
            Out::SetChannel(c) => s.channel = c,
            Out::NowPlaying(data) => {
                if let Some(old) = s.now_playing.take() {
                    delete(&deps.http, old).await;
                }
                let msg = now_playing_card(&deps, &data).await;
                if let Some(ch) = s.channel {
                    s.now_playing = post(&deps.http, ch, &msg, &mut s.channel).await.map(|id| (ch, id));
                }
            }
            Out::RefreshNowPlaying(data) => {
                if let Some((ch, id)) = s.now_playing {
                    let msg = now_playing_card(&deps, &data).await;
                    let body = msg.to_json();
                    let edit = deps
                        .http
                        .edit_message(ChannelId::new(ch), MessageId::new(id), &body, vec![]);
                    if !matches!(tokio::time::timeout(HTTP_TIMEOUT, edit).await, Ok(Ok(_))) {
                        debug!("could not edit the now playing message");
                    }
                }
            }
            Out::ClearNowPlaying => {
                if let Some(old) = s.now_playing.take() {
                    delete(&deps.http, old).await;
                }
            }
            Out::Notice(n) => {
                if let Some(old) = s.status.take() {
                    delete(&deps.http, old).await;
                }
                let msg = cards::notice(&n.card(deps.ai_enabled), deps.voice_channel_id, &deps.i18n);
                if let Some(ch) = s.channel {
                    s.status = post(&deps.http, ch, &msg, &mut s.channel).await.map(|id| (ch, id));
                }
            }
            Out::Close {
                delete_now_playing,
                done,
            } => {
                if delete_now_playing {
                    if let Some(old) = s.now_playing.take() {
                        delete(&deps.http, old).await;
                    }
                }
                let _ = done.send(());
                return;
            }
        }
    }
}

async fn now_playing_card(deps: &MessengerDeps, data: &NpData) -> Message {
    let info = &data.entry.info;
    let genres = match tokio::time::timeout(
        Duration::from_secs(2),
        deps.store.track_genres_by_key(&track_key(info), 4),
    )
    .await
    {
        Ok(Ok(g)) => g,
        _ => Vec::new(),
    };
    let tag = track_tag(&data.entry.uid);
    let view = NowPlaying {
        track: TrackCard {
            title: &info.title,
            url: &info.url,
            artists: &info.artists,
            duration_ms: info.duration_ms,
            thumbnail: info.thumbnail.as_deref(),
            requester_id: data.entry.requester.id,
        },
        voice_channel_id: deps.voice_channel_id,
        node_name: data.node_name.as_deref(),
        genres: &genres,
        paused: data.paused,
        ai_enabled: deps.ai_enabled,
        track_tag: &tag,
    };
    cards::now_playing(&view, &deps.i18n)
}

/// Discord error codes that mean the text channel cannot be used any more.
const CHANNEL_GONE: [isize; 3] = [10003, 50001, 50013];

pub fn discord_error_code(e: &serenity::Error) -> Option<isize> {
    match e {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(r)) => Some(r.error.code),
        _ => None,
    }
}

/// Send a message to a text channel; on "channel gone / no access" the channel is dropped so the
/// player stops trying (the next /play sets a new one).
async fn post(http: &Http, channel: u64, msg: &Message, slot: &mut Option<u64>) -> Option<u64> {
    match tokio::time::timeout(
        HTTP_TIMEOUT,
        http.send_message(ChannelId::new(channel), vec![], &msg.to_json()),
    )
    .await
    {
        Ok(Ok(m)) => Some(m.id.get()),
        Ok(Err(e)) => {
            if discord_error_code(&e).is_some_and(|c| CHANNEL_GONE.contains(&c)) {
                debug!(channel, "text channel unusable, notices disabled");
                *slot = None;
            } else {
                warn!(channel, error = %e, "could not send a player message");
            }
            None
        }
        Err(_) => {
            warn!(channel, "sending a player message timed out");
            None
        }
    }
}

async fn delete(http: &Http, (channel, message): (u64, u64)) {
    let _ = tokio::time::timeout(
        HTTP_TIMEOUT,
        http.delete_message(ChannelId::new(channel), MessageId::new(message), None),
    )
    .await;
}
