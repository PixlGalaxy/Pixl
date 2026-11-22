//! `GET /api/player` and the player actions. Each action runs the same `can(...)` rule as the slash
//! commands, against the player of the channel in the body.

use std::collections::{HashMap, HashSet};

use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;

use super::{
    auth::Member,
    error::{snowflake, ApiError, ApiResult, Body, Params},
    media::{rewrite, rewrite_opt},
    ApiState,
};
use crate::{
    perms::{can, Action, Decision},
    player::{
        state::{ApiContext, ApiPlayerState},
        PlayerError, PlayerHandle, PlayerSnapshot, SkipOutcome,
    },
    store::{track_key, GuildSettings, LoopMode},
};

/// Genres are looked up for the current track, this many upcoming ones and the history.
const GENRE_UPCOMING: usize = 25;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelQuery {
    pub channel_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelBody {
    pub channel_id: String,
}

pub fn gone(e: PlayerError) -> ApiError {
    match e {
        PlayerError::Gone => ApiError::not_found("Nothing is playing in that channel"),
        PlayerError::Timeout => ApiError::bot_unavailable("The player is busy, try again"),
    }
}

/// A player action that passed the permission rule.
pub struct Allowed {
    pub player: PlayerHandle,
    pub settings: GuildSettings,
    pub decision: Decision,
}

impl ApiState {
    /// Permission rule first (so a denied user learns nothing more), then the player must exist.
    pub async fn allowed(
        &self,
        m: &Member,
        channel: u64,
        action: impl FnOnce(Option<&PlayerSnapshot>) -> Action,
    ) -> ApiResult<Allowed> {
        let player = self.app.players.get(m.guild_id, channel);
        let snap = player.as_ref().map(|p| p.snapshot());
        let requested_current = snap
            .as_ref()
            .and_then(|s| s.current.as_ref())
            .is_some_and(|e| e.requester.id == m.user_id());
        let action = action(snap.as_deref());
        let actor = self
            .actor(m.guild_id, m.user_id(), Some(channel), requested_current)
            .await?;
        let settings = self.app.store.guild_settings(m.guild_id).await.unwrap_or_default();
        let decision = can(&actor, action, &settings);
        if let Decision::Deny(d) = decision {
            return Err(ApiError::deny(d));
        }
        let player = player.ok_or_else(|| ApiError::not_found("Nothing is playing in that channel"))?;
        Ok(Allowed {
            player,
            settings,
            decision,
        })
    }

    /// Full panel state of a snapshot as `viewer` sees it.
    pub async fn api_state(&self, viewer: u64, snap: &PlayerSnapshot) -> ApiPlayerState {
        let guild = snap.guild_id;
        let mut ctx = ApiContext {
            channel_name: self.discord.channel_name(guild, snap.channel_id).unwrap_or_default(),
            bot_name: self.discord.bot_name(snap.bot_index),
            ..Default::default()
        };
        let all = snap
            .current
            .iter()
            .chain(snap.upcoming.iter())
            .chain(snap.history.iter());
        let mut seen = HashSet::new();
        for e in all.filter(|e| e.requester.name.is_none()) {
            if seen.insert(e.requester.id) {
                if let Some(p) = self.profile_or_fetch(guild, e.requester.id) {
                    ctx.users.insert(e.requester.id, (p.name, p.avatar_url));
                }
            }
        }
        let shown: Vec<_> = snap
            .current
            .iter()
            .chain(snap.upcoming.iter().take(GENRE_UPCOMING))
            .chain(snap.history.iter())
            .collect();
        let mut keys = Vec::with_capacity(shown.len());
        for e in &shown {
            let key = track_key(&e.info);
            if !ctx.genres.contains_key(&key) {
                let genres = match self.genres.get(&key) {
                    Some(g) => g,
                    None => {
                        let g = self.app.ai.genres_for(&e.info).await;
                        self.genres.insert(key.clone(), g.clone());
                        g
                    }
                };
                if !genres.is_empty() {
                    ctx.genres.insert(key.clone(), genres);
                }
            }
            keys.push(key);
        }
        if self.app.config.ai_enabled {
            ctx.feedback = self
                .app
                .store
                .feedback_for_keys(viewer, &keys)
                .await
                .unwrap_or_else(|_| HashMap::new());
        }
        let mut state = snap.to_api(&ctx);
        for t in state
            .current
            .iter_mut()
            .chain(state.upcoming.iter_mut())
            .chain(state.history.iter_mut())
        {
            t.artwork_url = rewrite_opt(t.artwork_url.take());
            t.requested_by.avatar_url = t.requested_by.avatar_url.take().map(|u| rewrite(&u));
        }
        state
    }
}

pub async fn state(
    State(st): State<ApiState>,
    m: Member,
    Params(q): Params<ChannelQuery>,
) -> ApiResult<Json<Option<ApiPlayerState>>> {
    let channel = snowflake(&q.channel_id)?;
    let Some(p) = st.app.players.get(m.guild_id, channel) else {
        return Ok(Json(None));
    };
    let snap = p.snapshot();
    Ok(Json(Some(st.api_state(m.user_id(), &snap).await)))
}

async fn control(st: &ApiState, m: &Member, channel_id: &str) -> ApiResult<PlayerHandle> {
    let channel = snowflake(channel_id)?;
    Ok(st.allowed(m, channel, |_| Action::Control).await?.player)
}

pub async fn pause(State(st): State<ApiState>, m: Member, Body(b): Body<ChannelBody>) -> ApiResult<StatusCode> {
    control(&st, &m, &b.channel_id).await?.pause().await.map_err(gone)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn resume(State(st): State<ApiState>, m: Member, Body(b): Body<ChannelBody>) -> ApiResult<StatusCode> {
    control(&st, &m, &b.channel_id).await?.resume().await.map_err(gone)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn skip(State(st): State<ApiState>, m: Member, Body(b): Body<ChannelBody>) -> ApiResult<StatusCode> {
    let channel = snowflake(&b.channel_id)?;
    let a = st.allowed(&m, channel, |_| Action::Skip).await?;
    let force = a.decision == Decision::Allow;
    match a
        .player
        .skip(m.user_id(), force, a.settings.vote_skip)
        .await
        .map_err(gone)?
    {
        SkipOutcome::NothingPlaying => Err(ApiError::not_found("Nothing is playing")),
        _ => Ok(StatusCode::NO_CONTENT),
    }
}

pub async fn previous(State(st): State<ApiState>, m: Member, Body(b): Body<ChannelBody>) -> ApiResult<StatusCode> {
    match control(&st, &m, &b.channel_id).await?.previous().await.map_err(gone)? {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(ApiError::not_found("No previous track")),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeekBody {
    channel_id: String,
    position_ms: u64,
}

pub async fn seek(State(st): State<ApiState>, m: Member, Body(b): Body<SeekBody>) -> ApiResult<StatusCode> {
    if control(&st, &m, &b.channel_id)
        .await?
        .seek(b.position_ms)
        .await
        .map_err(gone)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::invalid("This track can't be seeked"))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeBody {
    channel_id: String,
    volume: u16,
}

pub async fn volume(State(st): State<ApiState>, m: Member, Body(b): Body<VolumeBody>) -> ApiResult<StatusCode> {
    if b.volume > crate::store::MAX_VOLUME {
        return Err(ApiError::invalid("volume must be 0-200"));
    }
    control(&st, &m, &b.channel_id)
        .await?
        .set_volume(b.volume)
        .await
        .map_err(gone)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopBody {
    channel_id: String,
    mode: LoopMode,
}

pub async fn set_loop(State(st): State<ApiState>, m: Member, Body(b): Body<LoopBody>) -> ApiResult<StatusCode> {
    control(&st, &m, &b.channel_id)
        .await?
        .set_loop(b.mode)
        .await
        .map_err(gone)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn shuffle(State(st): State<ApiState>, m: Member, Body(b): Body<ChannelBody>) -> ApiResult<StatusCode> {
    control(&st, &m, &b.channel_id).await?.shuffle().await.map_err(gone)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn stop(State(st): State<ApiState>, m: Member, Body(b): Body<ChannelBody>) -> ApiResult<StatusCode> {
    control(&st, &m, &b.channel_id)
        .await?
        .stop(m.user_id(), true)
        .await
        .map_err(gone)?;
    Ok(StatusCode::NO_CONTENT)
}
