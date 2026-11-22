//! AI radio routes: start, stop and cancel, on the shared entry points of `player::radio` (the same
//! rules as `/ai`: AI enabled, in the channel, history on, `canAdd` to start, owner or `canControl`
//! to stop / cancel).

use axum::{extract::State, http::StatusCode};
use serde::Deserialize;
use uuid::Uuid;

use super::{
    auth::Member,
    error::{snowflake, ApiError, ApiResult, Body},
    ApiState,
};
use crate::{
    ai::RadioMix,
    player::{
        radio::{self, RadioApiError, RadioStartRequest},
        Requester,
    },
};

pub fn radio_error(e: RadioApiError) -> ApiError {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    ApiError::new(status, e.code(), e.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartBody {
    channel_id: String,
    #[serde(default)]
    prompt: Option<String>,
    /// Optional, like the `/ai` options (default: balanced, blended with the listeners).
    #[serde(default)]
    mix: Option<RadioMix>,
    #[serde(default)]
    blend: Option<bool>,
}

pub async fn start(State(st): State<ApiState>, m: Member, Body(b): Body<StartBody>) -> ApiResult<StatusCode> {
    let channel = snowflake(&b.channel_id)?;
    if b.prompt.as_deref().is_some_and(|p| p.len() > 500) {
        return Err(ApiError::invalid("prompt is too long"));
    }
    let s = &m.session;
    radio::start_radio(
        &st.app.players,
        RadioStartRequest {
            guild_id: m.guild_id,
            voice_channel_id: channel,
            user: Requester {
                id: s.user_id,
                name: Some(s.display_name().to_owned()),
                avatar_url: s.avatar_url.clone(),
            },
            interaction_permissions: None,
            text_channel_id: None,
            prompt: b.prompt,
            mix: b.mix.unwrap_or_default(),
            blend: b.blend.unwrap_or(true),
        },
    )
    .await
    .map_err(radio_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopBody {
    channel_id: String,
}

pub async fn stop(State(st): State<ApiState>, m: Member, Body(b): Body<StopBody>) -> ApiResult<StatusCode> {
    let channel = snowflake(&b.channel_id)?;
    radio::stop_radio(&st.app.players, m.guild_id, channel, m.user_id(), None)
        .await
        .map_err(radio_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelBody {
    channel_id: String,
    job_id: String,
}

pub async fn cancel(State(st): State<ApiState>, m: Member, Body(b): Body<CancelBody>) -> ApiResult<StatusCode> {
    let channel = snowflake(&b.channel_id)?;
    let job = Uuid::parse_str(b.job_id.trim()).map_err(|_| ApiError::invalid("invalid jobId"))?;
    radio::cancel_radio_job(&st.app.players, m.guild_id, channel, m.user_id(), None, Some(job))
        .await
        .map_err(radio_error)?;
    Ok(StatusCode::NO_CONTENT)
}
