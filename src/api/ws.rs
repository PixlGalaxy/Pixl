//! `GET /ws?session=&channelId=`: live player state for one voice channel.
//!
//! On connect the full `state` goes out (null when nothing plays; the socket then waits for a player
//! to appear). Player events become `state` / `position` messages, guild settings changes become
//! `settings`, and permission changes `permissions`. Writes go through a small bounded queue; a
//! client that can't keep up, stops pinging, or whose session expired is disconnected.

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
    error::{snowflake, ApiError, Params},
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsQuery {
    session: String,
    channel_id: String,
}

pub async fn upgrade(
    State(st): State<ApiState>,
    Params(q): Params<WsQuery>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    // Authenticate before looking at the upgrade, so a bad session never gets a socket.
    let Some(session) = st.sessions.get(&q.session) else {
        return ApiError::unauthorized().into_response();
    };
    let Some(guild) = session.guild_id else {
        return ApiError::invalid("Open the panel from a server").into_response();
    };
    let channel = match snowflake(&q.channel_id) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
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
        .on_upgrade(move |socket| run(st, token, session.user_id, guild, channel, socket))
}

/// Next event of the attached player, or never.
async fn next_event(
    player: &mut Option<(PlayerHandle, broadcast::Receiver<PlayerEvent>)>,
) -> Result<PlayerEvent, broadcast::error::RecvError> {
    match player {
        Some((_, rx)) => rx.recv().await,
        None => std::future::pending().await,
    }
}

struct Conn {
    st: ApiState,
    out: mpsc::Sender<String>,
    user: u64,
    guild: u64,
    channel: u64,
    permissions: Option<Permissions>,
}

impl Conn {
    /// Queue a message; false when the client is too slow (queue full) or gone.
    fn push(&self, v: Value) -> bool {
        self.out.try_send(v.to_string()).is_ok()
    }

    async fn state(&self, player: Option<&PlayerHandle>) -> bool {
        let state = match player {
            Some(p) => {
                let snap = p.snapshot();
                serde_json::to_value(self.st.api_state(self.user, &snap).await).unwrap_or(Value::Null)
            }
            None => Value::Null,
        };
        self.push(json!({ "type": "state", "state": state, "serverNow": epoch_ms() }))
    }

    /// Recompute permissions; send them when they changed. False when the socket should close.
    async fn permissions(&mut self) -> bool {
        match self.st.member_status(self.guild, self.user).await {
            Ok(s) if !s.member => return false,
            Ok(_) => {}
            Err(_) => return true, // Discord hiccup: keep the last known permissions
        }
        let Ok(p) = self.st.permissions(self.guild, self.user, Some(self.channel)).await else {
            return true;
        };
        if self.permissions == Some(p) {
            return true;
        }
        self.permissions = Some(p);
        self.push(json!({ "type": "permissions", "permissions": p }))
    }
}

async fn run(st: ApiState, token: String, user: u64, guild: u64, channel: u64, socket: WebSocket) {
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

    let mut conn = Conn {
        st: st.clone(),
        out,
        user,
        guild,
        channel,
        permissions: None,
    };
    let mut guild_events = st.events.subscribe();
    let mut player = st.app.players.get(guild, channel).map(|p| {
        let rx = p.subscribe();
        (p, rx)
    });
    let mut last_generation = None;
    let mut last_seen = Instant::now();
    let mut tick = tokio::time::interval(CHECK_EVERY);
    tick.tick().await;

    let mut open = conn.state(player.as_ref().map(|p| &p.0)).await && conn.permissions().await;
    while open {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => open = false,
                Some(Ok(_)) => last_seen = Instant::now(),
            },
            ev = next_event(&mut player) => {
                open = match ev {
                    Ok(PlayerEvent::State(snap)) if snap.channel_id == channel => {
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
            p = st.app.players.wait_player(guild, channel, last_generation), if player.is_none() => {
                let rx = p.subscribe();
                player = Some((p, rx));
                open = conn.state(player.as_ref().map(|p| &p.0)).await;
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
                open = st.sessions.get(&token).is_some()
                    && last_seen.elapsed() < IDLE_TIMEOUT
                    && conn.permissions().await;
            },
            _ = st.app.shutdown.cancelled() => open = false,
        }
    }
    drop(conn);
    let _ = tokio::time::timeout(WRITE_TIMEOUT, writer).await;
}
