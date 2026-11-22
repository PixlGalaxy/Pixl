//! `/api/jam/*`: start, preview, join, leave and manage jams (see `player::jam`).
//!
//! Every route acts on the user's current voice channel (the same rule as the player routes):
//! create, join and leave on the user's own channel, the host actions only from the host channel.
//! Permissions follow `perms::can_jam`.

use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use super::{
    auth::Member,
    error::{snowflake, ApiError, ApiResult, Body},
    player::{gone, ChannelBody},
    queue::play_error,
    ApiState,
};
use crate::{
    discord::play,
    perms::{can_jam, Decision, JamAction},
    player::jam::{self, CreateRefused, JamHandle, JamSettings, JamView, JoinRefused, SettingsPatch, MAX_MEMBERS},
    store::SessionKey,
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiJamMember {
    pub guild_id: String,
    pub channel_id: String,
    pub name: String,
    pub guild_name: String,
    pub is_host: bool,
    /// People are in the channel.
    pub listening: bool,
    /// The viewer's channel.
    pub you: bool,
}

/// What the viewer may do, relative to their own voice channel.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiJamYou {
    pub member: bool,
    pub is_host: bool,
    /// Settings, kick, close, end.
    pub can_manage: bool,
    /// Edit the shared queue (the jam's `editableBy` rule; the server's DJ mode still applies).
    pub can_edit: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiJam {
    pub id: String,
    pub host_guild_id: String,
    pub host_channel_id: String,
    pub host_name: String,
    pub host_guild_name: String,
    pub members: Vec<ApiJamMember>,
    pub max_members: usize,
    pub settings: JamSettings,
    /// Activity link with the jam's `custom_id`: whoever opens it can join from their own channel.
    pub share_link: String,
    pub you: ApiJamYou,
}

fn key(guild_id: u64, channel: u64) -> SessionKey {
    SessionKey {
        guild_id,
        voice_channel_id: channel,
    }
}

pub fn join_error(e: JoinRefused) -> ApiError {
    let status = match e {
        JoinRefused::Ended => StatusCode::NOT_FOUND,
        JoinRefused::ChannelsOff | JoinRefused::ServersOff | JoinRefused::Closed => StatusCode::FORBIDDEN,
        JoinRefused::Full | JoinRefused::AlreadyIn | JoinRefused::InAnotherJam => StatusCode::CONFLICT,
    };
    ApiError::new(status, e.code(), e.to_string())
}

fn not_in_jam() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_in_jam", "This channel is not in a jam")
}

fn jam_gone() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "jam_ended", "That jam ended")
}

impl ApiState {
    /// The `Jam` object as `user`, in `viewer_channel` of `guild`, sees it.
    pub async fn api_jam(&self, user: u64, guild: u64, viewer_channel: Option<u64>, view: &JamView) -> ApiJam {
        let me = viewer_channel.map(|c| key(guild, c));
        let is_host = me == Some(view.host);
        let can_manage = if is_host {
            let settings = self.app.store.guild_settings(guild).await.unwrap_or_default();
            match self.actor(guild, user, viewer_channel, false).await {
                Ok(a) => can_jam(&a, JamAction::Manage, &settings) == Decision::Allow,
                Err(_) => false,
            }
        } else {
            false
        };
        let host = view.host_member();
        ApiJam {
            id: view.id.to_string(),
            host_guild_id: view.host.guild_id.to_string(),
            host_channel_id: view.host.voice_channel_id.to_string(),
            host_name: host.map(|m| m.name.clone()).unwrap_or_default(),
            host_guild_name: host.map(|m| m.guild_name.clone()).unwrap_or_default(),
            members: view
                .members
                .iter()
                .map(|m| ApiJamMember {
                    guild_id: m.key.guild_id.to_string(),
                    channel_id: m.key.voice_channel_id.to_string(),
                    name: m.name.clone(),
                    guild_name: m.guild_name.clone(),
                    is_host: m.key == view.host,
                    listening: m.has_humans,
                    you: Some(m.key) == me,
                })
                .collect(),
            max_members: MAX_MEMBERS,
            settings: view.settings,
            share_link: view.share_link(&self.app.config.discord_client_id),
            you: ApiJamYou {
                member: me.is_some_and(|k| view.is_member(k)),
                is_host,
                can_manage,
                can_edit: me.is_some_and(|k| view.may_edit(k)),
            },
        }
    }

    /// The jam of the user's channel, when that channel hosts it and the user may manage it.
    async fn managed_jam(&self, m: &Member, channel_id: &str) -> ApiResult<(JamHandle, u64)> {
        let channel = self.own_channel(m, channel_id)?;
        let jam = self
            .app
            .players
            .jams()
            .of(key(m.guild_id, channel))
            .ok_or_else(not_in_jam)?;
        if jam.view().host != key(m.guild_id, channel) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "jam_not_host",
                "Only the host channel manages the jam",
            ));
        }
        self.jam_permission(m, channel, JamAction::Manage).await?;
        Ok((jam, channel))
    }

    async fn jam_permission(&self, m: &Member, channel: u64, action: JamAction) -> ApiResult<bool> {
        let actor = self.actor(m.guild_id, m.user_id(), Some(channel), false).await?;
        let settings = self.app.store.guild_settings(m.guild_id).await.unwrap_or_default();
        match can_jam(&actor, action, &settings) {
            Decision::Deny(d) => Err(ApiError::deny(d)),
            _ => Ok(actor.is_manager),
        }
    }
}

#[derive(Serialize)]
pub struct JamResponse {
    jam: Option<ApiJam>,
}

/// `GET /api/jam?channelId=`: the jam of the user's channel (null when none).
pub async fn get(
    State(st): State<ApiState>,
    m: Member,
    super::error::Params(q): super::error::Params<super::player::ChannelQuery>,
) -> ApiResult<Json<JamResponse>> {
    let channel = st.own_channel(&m, &q.channel_id)?;
    let jam = match st.app.players.jams().of(key(m.guild_id, channel)) {
        Some(j) => Some(st.api_jam(m.user_id(), m.guild_id, Some(channel), &j.view()).await),
        None => None,
    };
    Ok(Json(JamResponse { jam }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinCheck {
    allowed: bool,
    /// Why not (an error code), when not allowed.
    code: Option<&'static str>,
}

#[derive(Serialize)]
pub struct PreviewResponse {
    jam: ApiJam,
    join: JoinCheck,
}

/// `GET /api/jam/{id}`: a jam opened from a link (the panel's join prompt), and whether the user's
/// current channel may join it.
pub async fn preview(State(st): State<ApiState>, m: Member, Path(id): Path<String>) -> ApiResult<Json<PreviewResponse>> {
    let id = jam::parse_id(&id).ok_or_else(|| ApiError::invalid("invalid jam id"))?;
    let handle = st.app.players.jams().get(id).ok_or_else(jam_gone)?;
    let view = handle.view();
    let channel = st.followed_channel(m.guild_id, m.user_id());
    let code = match channel {
        None => Some("not_in_channel"),
        Some(c) => {
            let k = key(m.guild_id, c);
            match st.app.players.jams().of(k) {
                Some(other) if other.id == handle.id => Some(JoinRefused::AlreadyIn.code()),
                Some(_) => Some(JoinRefused::InAnotherJam.code()),
                None => view.can_join(k).err().map(JoinRefused::code),
            }
        }
    };
    let jam = st.api_jam(m.user_id(), m.guild_id, channel, &view).await;
    Ok(Json(PreviewResponse {
        jam,
        join: JoinCheck {
            allowed: code.is_none(),
            code,
        },
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateResponse {
    jam_id: String,
    share_link: String,
    jam: ApiJam,
}

/// `POST /api/jam/create { channelId }`: host a jam from the user's channel (or get the one it is
/// in). The channel must have a player.
pub async fn create(State(st): State<ApiState>, m: Member, Body(b): Body<ChannelBody>) -> ApiResult<Json<CreateResponse>> {
    let channel = st.own_channel(&m, &b.channel_id)?;
    st.jam_permission(&m, channel, JamAction::Create).await?;
    let player = st
        .app
        .players
        .get(m.guild_id, channel)
        .ok_or_else(|| ApiError::not_found("Nothing is playing in that channel"))?;
    let jams = st.app.players.jams();
    let k = key(m.guild_id, channel);
    let handle = match jams.of(k) {
        Some(j) => j,
        None => match player.jam_create().await.map_err(gone)? {
            Ok(j) => j,
            Err(CreateRefused::AlreadyInJam) => jams.of(k).ok_or_else(not_in_jam)?,
            Err(CreateRefused::TooMany) => return Err(ApiError::unavailable("Too many jams right now, try later")),
        },
    };
    let view = handle.view();
    Ok(Json(CreateResponse {
        jam_id: view.id.to_string(),
        share_link: view.share_link(&st.app.config.discord_client_id),
        jam: st.api_jam(m.user_id(), m.guild_id, Some(channel), &view).await,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinBody {
    channel_id: String,
    jam_id: String,
}

/// `POST /api/jam/join { channelId, jamId }`: make the user's channel a member. A free bot joins
/// the channel when no Lynx is there (the usual "every bot is busy" error otherwise); the
/// channel's own queue is saved and comes back when it leaves.
pub async fn join(State(st): State<ApiState>, m: Member, Body(b): Body<JoinBody>) -> ApiResult<Json<JamResponse>> {
    let channel = st.own_channel(&m, &b.channel_id)?;
    let id = jam::parse_id(&b.jam_id).ok_or_else(|| ApiError::invalid("invalid jam id"))?;
    let jams = st.app.players.jams();
    let handle = jams.get(id).ok_or_else(jam_gone)?;
    let k = key(m.guild_id, channel);
    match jams.of(k) {
        Some(other) if other.id == handle.id => return Err(join_error(JoinRefused::AlreadyIn)),
        Some(_) => return Err(join_error(JoinRefused::InAnotherJam)),
        None => {}
    }
    handle.view().can_join(k).map_err(join_error)?;
    let existing = st.app.players.get(m.guild_id, channel);
    let replaces_queue = existing.as_ref().is_some_and(|p| {
        let s = p.snapshot();
        s.current.is_some() || !s.upcoming.is_empty()
    });
    let is_manager = st
        .jam_permission(&m, channel, JamAction::Join { replaces_queue })
        .await?;
    st.check_voice_channel(m.guild_id, channel, is_manager).await?;
    let created = existing.is_none();
    let player = match existing {
        Some(p) => p,
        None => {
            let ctx = st.play_ctx();
            play::precheck(ctx, m.guild_id, channel).map_err(play_error)?;
            play::player_for(ctx, m.guild_id, channel, None, m.user_id(), None)
                .await
                .map_err(play_error)?
        }
    };
    match player.jam_join(handle.clone()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            // A bot that joined only for this leaves again.
            if created {
                let _ = player.stop(m.user_id(), false).await;
            }
            return Err(join_error(e));
        }
        Err(e) => return Err(gone(e)),
    }
    // The jam publishes the view with the new member right after answering.
    let mut rx = handle.subscribe();
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while !rx.borrow_and_update().is_member(k) {
            if rx.changed().await.is_err() {
                break;
            }
        }
    })
    .await;
    let view = handle.view();
    Ok(Json(JamResponse {
        jam: Some(st.api_jam(m.user_id(), m.guild_id, Some(channel), &view).await),
    }))
}

/// `POST /api/jam/leave { channelId }`: take the user's channel out of its jam.
pub async fn leave(State(st): State<ApiState>, m: Member, Body(b): Body<ChannelBody>) -> ApiResult<StatusCode> {
    let channel = st.own_channel(&m, &b.channel_id)?;
    st.app
        .players
        .jams()
        .of(key(m.guild_id, channel))
        .ok_or_else(not_in_jam)?;
    st.jam_permission(&m, channel, JamAction::Leave).await?;
    let player = st.app.players.get(m.guild_id, channel).ok_or_else(not_in_jam)?;
    if player.jam_leave().await.map_err(gone)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_in_jam())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsBody {
    channel_id: String,
    #[serde(flatten)]
    patch: SettingsPatch,
}

#[derive(Serialize)]
pub struct SettingsResponse {
    settings: JamSettings,
}

/// `POST /api/jam/settings { channelId, allowChannels?, allowServers?, editableBy?, closed? }`.
pub async fn settings(
    State(st): State<ApiState>,
    m: Member,
    Body(b): Body<SettingsBody>,
) -> ApiResult<Json<SettingsResponse>> {
    let (jam, _) = st.managed_jam(&m, &b.channel_id).await?;
    let settings = jam.settings(b.patch).await.ok_or_else(jam_gone)?;
    Ok(Json(SettingsResponse { settings }))
}

/// `POST /api/jam/close { channelId }`: no new channels may join.
pub async fn close(State(st): State<ApiState>, m: Member, Body(b): Body<ChannelBody>) -> ApiResult<Json<SettingsResponse>> {
    let (jam, _) = st.managed_jam(&m, &b.channel_id).await?;
    let patch = SettingsPatch {
        closed: Some(true),
        ..Default::default()
    };
    let settings = jam.settings(patch).await.ok_or_else(jam_gone)?;
    Ok(Json(SettingsResponse { settings }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KickBody {
    channel_id: String,
    guild_id: String,
    member_channel_id: String,
}

/// `POST /api/jam/kick { channelId, guildId, memberChannelId }`: remove another channel.
pub async fn kick(State(st): State<ApiState>, m: Member, Body(b): Body<KickBody>) -> ApiResult<StatusCode> {
    let (jam, _) = st.managed_jam(&m, &b.channel_id).await?;
    let target = key(snowflake(&b.guild_id)?, snowflake(&b.member_channel_id)?);
    if target == jam.view().host {
        return Err(ApiError::invalid("The host channel cannot be removed; end the jam instead"));
    }
    if jam.kick(target).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("That channel is not in the jam"))
    }
}

/// `POST /api/jam/end { channelId }`: every channel goes back to its own queue.
pub async fn end(State(st): State<ApiState>, m: Member, Body(b): Body<ChannelBody>) -> ApiResult<StatusCode> {
    let (jam, _) = st.managed_jam(&m, &b.channel_id).await?;
    jam.end();
    Ok(StatusCode::NO_CONTENT)
}
