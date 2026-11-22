//! Guild settings (readable by every member, writable by managers), channel restrictions, DJs and
//! the DJ candidate search.

use std::collections::HashSet;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::info;

use super::{
    auth::Member,
    error::{snowflake, ApiError, ApiResult, Body, Params},
    media::rewrite_opt,
    upstream::{ChannelKind, GuildChannel},
    ApiState, GuildEvent,
};
use crate::{
    perms::channel_allowed,
    store::{ChannelRules, GuildSettings, GuildSettingsPatch, RuleKind, MAX_RULE_CHANNELS},
};

const MAX_DJS_SHOWN: usize = 100;
const MAX_CANDIDATES: usize = 25;

/// `GuildSettings` of the API: the stored settings plus the channel restrictions.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    #[serde(flatten)]
    pub settings: GuildSettings,
    pub command_channels: Vec<String>,
    pub voice_channels: Vec<String>,
}

impl SettingsView {
    fn new(settings: GuildSettings, rules: ChannelRules) -> Self {
        let ids = |v: Vec<u64>| v.into_iter().map(|c| c.to_string()).collect();
        Self {
            settings,
            command_channels: ids(rules.command_channels),
            voice_channels: ids(rules.voice_channels),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    #[serde(flatten)]
    base: GuildSettingsPatch,
    #[serde(default)]
    command_channels: Option<Vec<String>>,
    #[serde(default)]
    voice_channels: Option<Vec<String>>,
}

fn require_manager(m: &Member) -> ApiResult<()> {
    if m.status.manager {
        Ok(())
    } else {
        Err(ApiError::no_permission())
    }
}

impl ApiState {
    pub async fn settings_view(&self, guild_id: u64) -> ApiResult<SettingsView> {
        let store = &self.app.store;
        let (s, r) = tokio::join!(store.guild_settings(guild_id), store.channel_rules(guild_id));
        Ok(SettingsView::new(
            s.map_err(|e| ApiError::internal("could not read settings", e))?,
            r.map_err(|e| ApiError::internal("could not read channel rules", e))?,
        ))
    }

    /// Panel actions that make Lynx play in `channel` respect the voice restriction.
    pub async fn check_voice_channel(&self, guild_id: u64, channel: u64, is_manager: bool) -> ApiResult<()> {
        let rules = self.app.store.channel_rules(guild_id).await.unwrap_or_default();
        if channel_allowed(&rules.voice_channels, channel, is_manager) {
            Ok(())
        } else {
            Err(ApiError::channel_not_allowed())
        }
    }
}

pub async fn get_settings(State(st): State<ApiState>, m: Member) -> ApiResult<Json<SettingsView>> {
    Ok(Json(st.settings_view(m.guild_id).await?))
}

/// Parse and check a channel list against the guild's channels of the allowed kinds.
fn channel_list(raw: &[String], channels: &[GuildChannel], kinds: &[ChannelKind]) -> ApiResult<Vec<u64>> {
    if raw.len() > MAX_RULE_CHANNELS {
        return Err(ApiError::invalid(format!("at most {MAX_RULE_CHANNELS} channels")));
    }
    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        let id = snowflake(r)?;
        if !channels.iter().any(|c| c.id == id && kinds.contains(&c.kind)) {
            return Err(ApiError::invalid("unknown channel"));
        }
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

pub async fn put_settings(
    State(st): State<ApiState>,
    m: Member,
    Body(patch): Body<SettingsPatch>,
) -> ApiResult<Json<SettingsView>> {
    require_manager(&m)?;
    let g = m.guild_id;
    let store = &st.app.store;

    // Validate everything before writing anything.
    let mut next = store.guild_settings(g).await.unwrap_or_default();
    next.apply(&patch.base);
    next.validate().map_err(ApiError::invalid)?;
    let channels = if patch.command_channels.is_some() || patch.voice_channels.is_some() {
        st.discord.guild_channels(g)
    } else {
        Vec::new()
    };
    let commands = patch
        .command_channels
        .as_deref()
        .map(|l| channel_list(l, &channels, &[ChannelKind::Text, ChannelKind::Voice]))
        .transpose()?;
    let voice = patch
        .voice_channels
        .as_deref()
        .map(|l| channel_list(l, &channels, &[ChannelKind::Voice]))
        .transpose()?;

    let settings = store
        .update_guild_settings(g, &patch.base)
        .await
        .map_err(|e| ApiError::internal("could not save settings", e))?;
    if let Some(list) = commands {
        store
            .set_channel_rules(g, RuleKind::Command, &list)
            .await
            .map_err(|e| ApiError::internal("could not save channel rules", e))?;
    }
    if let Some(list) = voice {
        store
            .set_channel_rules(g, RuleKind::Voice, &list)
            .await
            .map_err(|e| ApiError::internal("could not save channel rules", e))?;
    }
    for p in st.app.players.in_guild(g) {
        let _ = p.settings_changed(settings).await;
    }
    let view = st.settings_view(g).await?;
    info!(guild = g, user = m.user_id(), "settings changed from the panel");
    if let Ok(v) = serde_json::to_value(&view) {
        let _ = st.events.send(GuildEvent::Settings(g, v));
    }
    Ok(Json(view))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiMember {
    pub id: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

#[derive(Serialize)]
pub struct DjList {
    djs: Vec<ApiMember>,
}

pub async fn list_djs(State(st): State<ApiState>, m: Member) -> ApiResult<Json<DjList>> {
    let list = st
        .app
        .store
        .list_djs(m.guild_id)
        .await
        .map_err(|e| ApiError::internal("could not read DJs", e))?;
    let mut set = tokio::task::JoinSet::new();
    for (i, d) in list.into_iter().take(MAX_DJS_SHOWN).enumerate() {
        let st = st.clone();
        let g = m.guild_id;
        set.spawn(async move { (i, d.user_id, st.profile(g, d.user_id).await) });
    }
    let mut out = Vec::new();
    while let Some(Ok(r)) = set.join_next().await {
        out.push(r);
    }
    out.sort_by_key(|r| r.0);
    Ok(Json(DjList {
        djs: out
            .into_iter()
            .map(|(_, id, p)| ApiMember {
                id: id.to_string(),
                name: p.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| id.to_string()),
                avatar_url: rewrite_opt(p.and_then(|p| p.avatar_url)),
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddDjBody {
    user_id: String,
}

pub async fn add_dj(State(st): State<ApiState>, m: Member, Body(b): Body<AddDjBody>) -> ApiResult<StatusCode> {
    require_manager(&m)?;
    let user = snowflake(&b.user_id)?;
    match st.discord.member(m.guild_id, user).await {
        Ok(Some(p)) => st.profiles.insert(user, p),
        Ok(None) => return Err(ApiError::not_found("That user is not in this server")),
        Err(_) => return Err(ApiError::bot_unavailable("Discord did not answer, try again")),
    }
    st.app
        .store
        .add_dj(m.guild_id, user, m.user_id())
        .await
        .map_err(|e| ApiError::internal("could not add DJ", e))?;
    let _ = st.events.send(GuildEvent::User(user));
    Ok(StatusCode::NO_CONTENT)
}

pub async fn remove_dj(State(st): State<ApiState>, m: Member, Path(user_id): Path<String>) -> ApiResult<StatusCode> {
    require_manager(&m)?;
    let user = snowflake(&user_id)?;
    let removed = st
        .app
        .store
        .remove_dj(m.guild_id, user)
        .await
        .map_err(|e| ApiError::internal("could not remove DJ", e))?;
    if !removed {
        return Err(ApiError::not_found("That user is not a DJ"));
    }
    let _ = st.events.send(GuildEvent::User(user));
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct CandidateQuery {
    #[serde(default)]
    q: String,
}

#[derive(Serialize)]
pub struct Candidates {
    members: Vec<ApiMember>,
}

/// People in the guild's voice channels, plus a member search when Discord allows it.
pub async fn candidates(
    State(st): State<ApiState>,
    m: Member,
    Params(q): Params<CandidateQuery>,
) -> ApiResult<Json<Candidates>> {
    require_manager(&m)?;
    let query = q.q.trim().to_lowercase();
    if query.len() > 100 {
        return Err(ApiError::invalid("search is too long"));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (id, profile) in st.discord.voice_members(m.guild_id) {
        let profile = profile.or_else(|| st.profile_or_fetch(m.guild_id, id));
        let name = profile
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| id.to_string());
        if !query.is_empty() && !name.to_lowercase().contains(&query) {
            continue;
        }
        if seen.insert(id) {
            out.push(ApiMember {
                id: id.to_string(),
                name,
                avatar_url: rewrite_opt(profile.and_then(|p| p.avatar_url)),
            });
        }
    }
    if !query.is_empty() && out.len() < MAX_CANDIDATES {
        match st
            .discord
            .search_members(m.guild_id, &query, MAX_CANDIDATES as u32)
            .await
        {
            Some(found) => {
                for (id, p) in found {
                    if seen.insert(id) {
                        out.push(ApiMember {
                            id: id.to_string(),
                            name: p.name,
                            avatar_url: rewrite_opt(p.avatar_url),
                        });
                    }
                }
            }
            None => {
                if !st
                    .search_refused_logged
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    info!("Discord refused the member search; DJ candidates come from voice channels only");
                }
            }
        }
    }
    out.truncate(MAX_CANDIDATES);
    Ok(Json(Candidates { members: out }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiChannelEntry {
    id: String,
    name: String,
    parent_name: Option<String>,
}

#[derive(Serialize)]
pub struct GuildChannels {
    text: Vec<ApiChannelEntry>,
    voice: Vec<ApiChannelEntry>,
}

/// `GET /api/guild/channels` (managers): channels for the restriction pickers.
pub async fn channels(State(st): State<ApiState>, m: Member) -> ApiResult<Json<GuildChannels>> {
    require_manager(&m)?;
    let mut out = GuildChannels {
        text: Vec::new(),
        voice: Vec::new(),
    };
    for c in st.discord.guild_channels(m.guild_id) {
        let entry = ApiChannelEntry {
            id: c.id.to_string(),
            name: c.name,
            parent_name: c.parent_name,
        };
        match c.kind {
            ChannelKind::Text => out.text.push(entry),
            ChannelKind::Voice => out.voice.push(entry),
        }
    }
    Ok(Json(out))
}

/// Settings messages for the sockets carry the same JSON as `GET /api/settings`.
pub fn settings_message(settings: &Value) -> Value {
    serde_json::json!({ "type": "settings", "settings": settings })
}
