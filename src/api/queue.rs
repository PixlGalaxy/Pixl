//! Queue routes: add (the same flow as `/play`), move, remove, clear, and Like / Dislike.

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    auth::Member,
    error::{ApiError, ApiResult, Body},
    player::{gone, ChannelBody},
    ApiState, GuildEvent,
};
use crate::{
    discord::play::{self, PlayCtx, PlayError, PlayRequest},
    perms::{can, Action, Decision},
    player::Requester,
    sources::SourceError,
    store::Feedback,
};

const MAX_QUERY: usize = 500;

fn parse_uid(raw: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(raw.trim()).map_err(|_| ApiError::invalid("invalid uid"))
}

impl ApiState {
    pub fn play_ctx(&self) -> PlayCtx<'_> {
        PlayCtx {
            players: &self.app.players,
            bots: &self.app.bots,
            store: &self.app.store,
            sources: &self.app.sources,
            max_playlists: self.app.config.max_stored_playlists,
        }
    }
}

pub fn play_error(e: PlayError) -> ApiError {
    match e {
        PlayError::NoResults => ApiError::no_results(),
        PlayError::Source(SourceError::NotFound(_)) => ApiError::no_results(),
        PlayError::Source(SourceError::Unsupported(r)) => ApiError::invalid(r),
        PlayError::Source(SourceError::SpotifyDisabled) => {
            ApiError::invalid("Spotify links are not enabled on this bot")
        }
        // Internal errors (I/O, process spawn) may carry paths: log them, show a generic text.
        PlayError::Source(SourceError::Other(e)) => {
            tracing::warn!(error = %e, "resolve failed");
            ApiError::new(StatusCode::BAD_GATEWAY, "no_results", "Could not load that")
        }
        PlayError::Source(e) => ApiError::new(StatusCode::BAD_GATEWAY, "no_results", e.to_string()),
        PlayError::Timeout => ApiError::new(StatusCode::GATEWAY_TIMEOUT, "no_results", "Resolving took too long"),
        PlayError::CannotJoin { .. } => ApiError::bot_unavailable("Lynx can't connect or speak in this channel"),
        PlayError::Busy { invite, .. } => ApiError::bot_unavailable("Every Lynx bot is busy in another channel")
            .with_invite(invite.map(|(_, url)| url)),
        PlayError::ShuttingDown => ApiError::bot_unavailable("Lynx is restarting, try again in a moment"),
        PlayError::JoinFailed => ApiError::bot_unavailable("Could not join the voice channel"),
        PlayError::Denied(d) => ApiError::deny(d),
        PlayError::VoiceNotAllowed { .. } => ApiError::channel_not_allowed(),
        PlayError::QueueFull { max } => ApiError::new(
            StatusCode::CONFLICT,
            "queue_full",
            format!("The queue is full ({max} tracks)"),
        ),
        PlayError::Gone => ApiError::not_found("The player ended"),
        PlayError::SettingsUnavailable => ApiError::bot_unavailable("Could not read the server settings, try again"),
        PlayError::JamHostOnly { .. } => ApiError::jam_host_only(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddBody {
    channel_id: String,
    query: String,
}

#[derive(Serialize)]
pub struct Added {
    pub added: usize,
}

pub async fn add(State(st): State<ApiState>, m: Member, Body(b): Body<AddBody>) -> ApiResult<Json<Added>> {
    let channel = st.own_channel(&m, &b.channel_id)?;
    let query = b.query.trim();
    if query.is_empty() || query.len() > MAX_QUERY {
        return Err(ApiError::invalid("query must be 1-500 characters"));
    }
    st.add_checks(&m, channel).await?;
    let played = play::play(st.play_ctx(), play_request(&m, channel, query))
        .await
        .map_err(play_error)?;
    Ok(Json(Added {
        added: played.push.added,
    }))
}

impl ApiState {
    /// Everything `/api/queue/add` checks before resolving: the Add permission (DJ mode), the
    /// voice channel restriction, the jam's host-only rule, the per-user add limit and a free bot.
    pub async fn add_checks(&self, m: &Member, channel: u64) -> ApiResult<()> {
        let actor = self.actor(m.guild_id, m.user_id(), Some(channel), false).await?;
        let settings = self.app.store.guild_settings(m.guild_id).await.unwrap_or_default();
        if let Decision::Deny(d) = can(&actor, Action::Add, &settings) {
            return Err(ApiError::deny(d));
        }
        self.check_voice_channel(m.guild_id, channel, actor.is_manager).await?;
        self.jam_edit_check(m.guild_id, channel)?;
        if !self.add_limit.allow(m.user_id()) {
            return Err(ApiError::rate_limited());
        }
        play::precheck(self.play_ctx(), m.guild_id, channel).map_err(play_error)
    }
}

/// The `/play` request of a panel user in `channel`.
pub fn play_request<'a>(m: &Member, channel: u64, query: &'a str) -> PlayRequest<'a> {
    let s = &m.session;
    PlayRequest {
        guild_id: m.guild_id,
        user_id: s.user_id,
        voice_channel_id: channel,
        query,
        requester: Requester {
            id: s.user_id,
            name: Some(s.display_name().to_owned()),
            avatar_url: s.avatar_url.clone(),
        },
        text_channel: None,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveBody {
    channel_id: String,
    uid: String,
    to_index: usize,
}

pub async fn move_track(State(st): State<ApiState>, m: Member, Body(b): Body<MoveBody>) -> ApiResult<StatusCode> {
    let channel = st.own_channel(&m, &b.channel_id)?;
    let uid = parse_uid(&b.uid)?;
    let a = st.allowed(&m, channel, |_| Action::Control).await?;
    if a.player.move_uid(uid, b.to_index).await.map_err(gone)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("That track is no longer in the queue"))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UidBody {
    channel_id: String,
    uid: String,
}

pub async fn remove(State(st): State<ApiState>, m: Member, Body(b): Body<UidBody>) -> ApiResult<StatusCode> {
    let channel = st.own_channel(&m, &b.channel_id)?;
    let uid = parse_uid(&b.uid)?;
    let user = m.user_id();
    let a = st
        .allowed(&m, channel, |snap| {
            let own = snap
                .and_then(|s| s.upcoming.iter().find(|e| e.uid == uid))
                .is_some_and(|e| e.requester.id == user);
            Action::Remove { own }
        })
        .await?;
    match a.player.remove_uid(uid).await.map_err(gone)? {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(ApiError::not_found("That track is no longer in the queue")),
    }
}

pub async fn clear(State(st): State<ApiState>, m: Member, Body(b): Body<ChannelBody>) -> ApiResult<StatusCode> {
    let channel = st.own_channel(&m, &b.channel_id)?;
    let a = st.allowed(&m, channel, |_| Action::Control).await?;
    a.player.clear().await.map_err(gone)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackBody {
    channel_id: String,
    uid: String,
    value: Option<Feedback>,
}

pub async fn feedback(State(st): State<ApiState>, m: Member, Body(b): Body<FeedbackBody>) -> ApiResult<StatusCode> {
    if !st.app.config.ai_enabled {
        return Err(ApiError::not_found("AI features are turned off on this bot"));
    }
    let channel = st.own_channel(&m, &b.channel_id)?;
    let uid = parse_uid(&b.uid)?;
    let user = m.user_id();
    // Votes only feed the radio, which needs history.
    if !st.app.store.history_enabled(user).await.unwrap_or(true) {
        return Err(ApiError::history_disabled());
    }
    let a = st.allowed_local(&m, channel, |_| Action::Feedback).await?;
    let snap = a.player.snapshot();
    let entry = snap
        .current
        .iter()
        .chain(snap.upcoming.iter())
        .chain(snap.history.iter())
        .find(|e| e.uid == uid)
        .ok_or_else(|| ApiError::not_found("That track is no longer in the queue"))?;
    st.app
        .store
        .set_feedback(user, &entry.info, b.value)
        .await
        .map_err(|e| ApiError::internal("could not save feedback", e))?;
    let _ = st.events.send(GuildEvent::User(user));
    Ok(StatusCode::NO_CONTENT)
}
