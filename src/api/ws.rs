//! `GET /ws?session=`: live player state of the voice channel the user is in.
//!
//! On connect `channel` (the followed voice channel, null when the user is not in voice) and the full
//! `state` go out (null when nothing plays; the socket then waits for a player to appear). When the
//! user joins, leaves or moves between voice channels a new `channel` message is pushed and the socket
//! switches to that channel's player (its `state` follows). Player events become `state` / `position`
//! messages, guild settings changes become `settings`, and permission changes `permissions`. Writes go
//! through a small bounded queue; a client that can't keep up, stops pinging, or whose session expired
//! is disconnected.

use std::time::{Duration, Instant};

use axum::{
    extract::{
        ws::{rejection::WebSocketUpgradeRejection, Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

use super::{
    error::{ApiError, Params},
    settings::settings_message,
    ApiState, GuildEvent,
};
use crate::{
    perms::Permissions,
    player::{state::epoch_ms, PlayerEvent, PlayerHandle},
};

/// The panel pings every 25 s; two missed pings plus slack closes the socket.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_secs(15);
const OUT_QUEUE: usize = 32;
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_IN_MESSAGE: usize = 4 * 1024;
const MAX_WRITE_BUFFER: usize = 8 * 1024 * 1024;

/// A `channelId` sent by older panels is ignored: the server follows the user's voice channel.
#[derive(Deserialize)]
pub struct WsQuery {
    session: String,
}

pub async fn upgrade(
    State(st): State<ApiState>,
    Params(q): Params<WsQuery>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    // Authenticate before looking at the upgrade, so a bad session never gets a socket.
    let Some(session) = st.sessions.get(&q.session).await else {
        return ApiError::unauthorized().into_response();
    };
    let Some(guild) = session.guild_id else {
        return ApiError::invalid("Open the panel from a server").into_response();
    };
    match st.member_status(guild, session.user_id).await {
        Ok(s) if s.member => {}
        Ok(_) => return ApiError::no_permission().into_response(),
        Err(e) => return e.into_response(),
    }
    let ws = match ws {
        Ok(ws) => ws,
        Err(e) => return e.into_response(),
    };
    let token = q.session;
    ws.max_message_size(MAX_IN_MESSAGE)
        .max_frame_size(MAX_IN_MESSAGE)
        .max_write_buffer_size(MAX_WRITE_BUFFER)
        .on_upgrade(move |socket| run(st, token, session.user_id, guild, socket))
}

type Attached = Option<(PlayerHandle, broadcast::Receiver<PlayerEvent>)>;

/// Next event of the attached player, or never.
async fn next_event(player: &mut Attached) -> Result<PlayerEvent, broadcast::error::RecvError> {
    match player {
        Some((_, rx)) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn attach(st: &ApiState, guild: u64, channel: Option<u64>) -> Attached {
    let p = st.app.players.get(guild, channel?)?;
    let rx = p.subscribe();
    Some((p, rx))
}

struct Conn {
    st: ApiState,
    out: mpsc::Sender<String>,
    user: u64,
    guild: u64,
    /// Followed voice channel: the user's current one.
    channel: Option<u64>,
    permissions: Option<Permissions>,
    /// Jam (id, rev) last sent, so a `jam` message goes out only when it changed.
    jam: Option<(String, u64)>,
}

impl Conn {
    /// Queue a message; false when the client is too slow (queue full) or gone.
    fn push(&self, v: Value) -> bool {
        self.out.try_send(v.to_string()).is_ok()
    }

    /// Full state, then a `jam` message when the channel's jam changed (joined, left, edited).
    async fn state(&mut self, player: Option<&PlayerHandle>) -> bool {
        let (state, jam_key) = match player {
            Some(p) => {
                let snap = p.snapshot();
                let key = snap.jam.as_ref().map(|j| (j.id.to_string(), j.rev));
                let state = serde_json::to_value(self.st.api_state(self.user, &snap).await).unwrap_or(Value::Null);
                (state, key)
            }
            None => (Value::Null, None),
        };
        let jam = state.get("jam").cloned().unwrap_or(Value::Null);
        if !self.push(json!({ "type": "state", "state": state, "serverNow": epoch_ms() })) {
            return false;
        }
        if jam_key == self.jam {
            return true;
        }
        self.jam = jam_key;
        self.push(json!({ "type": "jam", "jam": jam }))
    }

    fn channel_message(&self) -> bool {
        let channel = self.st.channel_ref(self.guild, self.channel);
        self.push(json!({ "type": "channel", "channel": channel }))
    }

    /// Re-resolve the followed channel. When it changed: tell the client, attach to that channel's
    /// player and send its state and permissions (always all three, in that order). False when the socket should close.
    async fn follow(&mut self, player: &mut Attached, last_generation: &mut Option<u64>) -> bool {
        let now = self.st.followed_channel(self.guild, self.user);
        if now == self.channel {
            return true;
        }
        self.channel = now;
        *player = attach(&self.st, self.guild, now);
        *last_generation = None;
        self.permissions = None; // always resend them: channel, state, permissions
        self.channel_message() && self.state(player.as_ref().map(|p| &p.0)).await && self.permissions().await
    }

    /// Recompute permissions; send them when they changed. False when the socket should close.
    async fn permissions(&mut self) -> bool {
        match self.st.member_status(self.guild, self.user).await {
            Ok(s) if !s.member => return false,
            Ok(_) => {}
            Err(_) => return true, // Discord hiccup: keep the last known permissions
        }
        let Ok(p) = self.st.permissions(self.guild, self.user, self.channel).await else {
            return true;
        };
        if self.permissions == Some(p) {
            return true;
        }
        self.permissions = Some(p);
        self.push(json!({ "type": "permissions", "permissions": p }))
    }
}

async fn run(st: ApiState, token: String, user: u64, guild: u64, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out, mut out_rx) = mpsc::channel::<String>(OUT_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            match tokio::time::timeout(WRITE_TIMEOUT, sink.send(Message::Text(text.into()))).await {
                Ok(Ok(())) => {}
                _ => return,
            }
        }
        let _ = tokio::time::timeout(WRITE_TIMEOUT, sink.close()).await;
    });

    // Subscribe before resolving, so a move in between is not missed.
    let mut voice_moves = st.app.bots.subscribe_voice_moves();
    let channel = st.followed_channel(guild, user);
    let mut conn = Conn {
        st: st.clone(),
        out,
        user,
        guild,
        channel,
        permissions: None,
        jam: None,
    };
    let mut guild_events = st.events.subscribe();
    let mut player = attach(&st, guild, channel);
    let mut last_generation = None;
    let mut last_seen = Instant::now();
    let mut tick = tokio::time::interval(CHECK_EVERY);
    tick.tick().await;

    let mut open =
        conn.channel_message() && conn.state(player.as_ref().map(|p| &p.0)).await && conn.permissions().await;
    while open {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => open = false,
                Some(Ok(_)) => last_seen = Instant::now(),
            },
            ev = next_event(&mut player) => {
                open = match ev {
                    Ok(PlayerEvent::State(snap)) if Some(snap.channel_id) == conn.channel => {
                        conn.state(player.as_ref().map(|p| &p.0)).await
                    }
                    Ok(PlayerEvent::Position { position_ms, position_at }) => conn.push(json!({
                        "type": "position",
                        "positionMs": position_ms,
                        "positionAt": position_at,
                        "serverNow": epoch_ms(),
                    })),
                    // Missed events: send the full state instead.
                    Err(broadcast::error::RecvError::Lagged(_)) => conn.state(player.as_ref().map(|p| &p.0)).await,
                    // Closed, or the bot moved to another channel.
                    Ok(PlayerEvent::State(_)) | Ok(PlayerEvent::Closed) | Err(broadcast::error::RecvError::Closed) => {
                        last_generation = player.take().map(|p| p.0.generation);
                        conn.state(None).await
                    }
                };
            },
            // A disabled branch still builds its future (never polled), hence the 0 placeholder.
            p = st.app.players.wait_player(guild, conn.channel.unwrap_or(0), last_generation),
                if player.is_none() && conn.channel.is_some() => {
                let rx = p.subscribe();
                player = Some((p, rx));
                open = conn.state(player.as_ref().map(|p| &p.0)).await;
            },
            ev = voice_moves.recv() => {
                open = match ev {
                    Ok((g, u)) if g == guild && u == user => conn.follow(&mut player, &mut last_generation).await,
                    Ok(_) => true,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        conn.follow(&mut player, &mut last_generation).await
                    }
                    Err(broadcast::error::RecvError::Closed) => false,
                };
            },
            ev = guild_events.recv() => {
                open = match ev {
                    Ok(GuildEvent::Settings(g, settings)) if g == guild => {
                        conn.push(settings_message(&settings)) && conn.permissions().await
                    }
                    Ok(GuildEvent::User(u)) if u == user => {
                        conn.state(player.as_ref().map(|p| &p.0)).await && conn.permissions().await
                    }
                    Ok(_) => true,
                    Err(broadcast::error::RecvError::Lagged(_)) => conn.permissions().await,
                    Err(broadcast::error::RecvError::Closed) => false,
                };
            },
            _ = tick.tick() => {
                // Also re-resolves the channel, in case a voice update was missed.
                open = st.sessions.get(&token).await.is_some()
                    && last_seen.elapsed() < IDLE_TIMEOUT
                    && conn.follow(&mut player, &mut last_generation).await
                    && conn.permissions().await;
            },
            _ = st.app.shutdown.cancelled() => open = false,
        }
    }
    drop(conn);
    let _ = tokio::time::timeout(WRITE_TIMEOUT, writer).await;
}
