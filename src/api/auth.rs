//! `GET /api/public`, `POST /api/auth`, the session extractors and the per-request permission
//! check (manager through Discord, DJ from the store, voice channel from the main bot's cache).

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use axum::{
    extract::{ConnectInfo, FromRequestParts, State},
    http::{request::Parts, HeaderMap},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use super::{
    error::{snowflake, ApiError, ApiResult, Body},
    media,
    session::Session,
    upstream::{Profile, UpstreamError},
    ApiState, MemberStatus,
};
use crate::perms::{Actor, Permissions};

pub async fn public(State(st): State<ApiState>) -> Json<Value> {
    let c = &st.app.config;
    let mut links = serde_json::Map::new();
    for (k, v) in [
        ("landing", &c.landing_url),
        ("terms", &c.terms_url),
        ("privacy", &c.privacy_url),
        ("support", &c.support_url),
    ] {
        if let Some(v) = v {
            links.insert(k.into(), Value::String(v.clone()));
        }
    }
    Json(json!({
        "clientId": c.discord_client_id,
        "features": { "ai": c.ai_enabled },
        "links": links,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginBody {
    code: String,
    #[serde(default)]
    guild_id: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiUser {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
    pub avatar_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiGuild {
    pub id: String,
    pub name: String,
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiChannel {
    pub id: String,
    pub name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    access_token: String,
    session: String,
    user: ApiUser,
    guild: Option<ApiGuild>,
    /// Voice channel the panel follows: the user's current one (null when not in voice).
    channel: Option<ApiChannel>,
    /// Channel the Activity was opened from (any kind), for display only.
    launch_channel: Option<ApiChannel>,
    permissions: Permissions,
    locale: Option<String>,
}

/// Client address for rate limits: `X-Real-IP`, which the bundled nginx always sets to the
/// address it trusts (`$remote_addr`, after `real_ip` when `TRUSTED_PROXIES` is configured), then
/// the socket. `X-Forwarded-For` is never read: its first hop is whatever the client sent.
/// IPv6 addresses count per /64, since one host usually controls a whole /64.
pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let real = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok());
    match real.or_else(|| peer.map(|p| p.ip())) {
        Some(IpAddr::V6(v6)) if v6.to_ipv4_mapped().is_none() => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        Some(IpAddr::V6(v6)) => v6.to_ipv4_mapped().map(|v4| v4.to_string()).unwrap_or_default(),
        Some(ip) => ip.to_string(),
        None => "unknown".into(),
    }
}

fn upstream(e: UpstreamError) -> ApiError {
    match e {
        UpstreamError::Rejected => ApiError::unauthorized(),
        UpstreamError::Unavailable(r) => {
            warn!(reason = %r, "discord request failed during sign-in");
            ApiError::bot_unavailable("Discord did not answer, try again")
        }
    }
}

pub async fn login(
    State(st): State<ApiState>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Body(body): Body<LoginBody>,
) -> ApiResult<Json<LoginResponse>> {
    if !st.auth_limit.allow(client_ip(&headers, peer)) {
        return Err(ApiError::rate_limited());
    }
    let code = body.code.trim();
    if code.is_empty() || code.len() > 512 {
        return Err(ApiError::invalid("missing code"));
    }
    let guild_id = body.guild_id.as_deref().map(snowflake).transpose()?;
    let channel_id = body.channel_id.as_deref().map(snowflake).transpose()?;

    let token = st.discord.exchange_code(code).await.map_err(upstream)?;
    let user = st.discord.current_user(&token).await.map_err(upstream)?;

    // Never trust the guild from the client: check membership through the bot.
    let (guild, launch, followed) = match guild_id {
        None => (None, None, None),
        Some(g) => {
            if st.discord.member(g, user.id).await.map_err(upstream)?.is_none() {
                return Err(ApiError::no_permission());
            }
            let summary = st.discord.guild(g);
            let guild = ApiGuild {
                id: g.to_string(),
                name: summary.as_ref().map(|s| s.name.clone()).unwrap_or_default(),
                icon_url: summary.and_then(|s| s.icon_url).map(|u| media::rewrite(&u)),
            };
            let launch = channel_id.filter(|c| st.discord.channel_name(g, *c).is_some());
            (Some((g, guild)), launch, st.followed_channel(g, user.id))
        }
    };

    let prefs = st.app.store.user_prefs(user.id).await.unwrap_or_default();
    let session = Session {
        user_id: user.id,
        username: user.username.clone(),
        global_name: user.global_name.clone(),
        avatar_url: user.avatar_url.clone(),
        guild_id: guild.as_ref().map(|g| g.0),
        launch_channel_id: launch,
        created_at: 0,
        expires_at: 0,
    };
    let permissions = match guild.as_ref() {
        Some((g, _)) => {
            st.members.remove(&(*g, user.id));
            st.permissions(*g, user.id, followed).await?
        }
        None => Permissions {
            is_manager: false,
            is_dj: false,
            can_control: false,
            can_add: false,
        },
    };
    let token_out = st.sessions.create(session).await;
    info!(user = user.id, guild = ?guild.as_ref().map(|g| g.0), "panel sign-in");
    Ok(Json(LoginResponse {
        access_token: token,
        session: token_out,
        user: ApiUser {
            id: user.id.to_string(),
            username: user.username,
            global_name: user.global_name,
            avatar_url: user.avatar_url.map(|u| media::rewrite(&u)),
        },
        channel: guild.as_ref().and_then(|(g, _)| st.channel_ref(*g, followed)),
        launch_channel: guild.as_ref().and_then(|(g, _)| st.channel_ref(*g, launch)),
        guild: guild.map(|g| g.1),
        permissions,
        locale: prefs.locale.or(user.locale),
    }))
}

impl ApiState {
    /// Membership and manager status, re-checked through Discord at most every `MEMBER_TTL`.
    pub async fn member_status(&self, guild_id: u64, user_id: u64) -> ApiResult<MemberStatus> {
        if let Some(s) = self.members.get(&(guild_id, user_id)) {
            return Ok(s);
        }
        let status = match self.discord.member(guild_id, user_id).await {
            Ok(Some(profile)) => {
                self.profiles.insert(user_id, profile);
                MemberStatus {
                    member: true,
                    manager: self.discord.is_manager(guild_id, user_id).await,
                }
            }
            Ok(None) => MemberStatus {
                member: false,
                manager: false,
            },
            Err(e) => {
                warn!(error = %e, guild = guild_id, "membership check failed");
                return Err(ApiError::bot_unavailable("Discord did not answer, try again"));
            }
        };
        self.members.insert((guild_id, user_id), status);
        Ok(status)
    }

    /// Permission actor of a member for the player in `channel` (None: no channel).
    pub async fn actor(
        &self,
        guild_id: u64,
        user_id: u64,
        channel: Option<u64>,
        requested_current: bool,
    ) -> ApiResult<Actor> {
        let status = self.member_status(guild_id, user_id).await?;
        let is_dj = self.app.store.is_dj(guild_id, user_id).await.unwrap_or(false);
        let in_channel = channel.is_some() && self.discord.user_voice_channel(guild_id, user_id) == channel;
        Ok(Actor {
            is_manager: status.manager,
            is_dj,
            in_channel,
            requested_current,
        })
    }

    pub async fn permissions(&self, guild_id: u64, user_id: u64, channel: Option<u64>) -> ApiResult<Permissions> {
        let actor = self.actor(guild_id, user_id, channel, false).await?;
        let settings = self.app.store.guild_settings(guild_id).await.unwrap_or_default();
        Ok(Permissions::of(&actor, &settings))
    }

    /// Cached profile of a user; on a miss, fetch it in the background (next state has it).
    pub fn profile_or_fetch(&self, guild_id: u64, user_id: u64) -> Option<Profile> {
        if let Some(p) = self.profiles.get(&user_id) {
            return Some(p);
        }
        if self.profile_pending.len() < 256 && self.profile_pending.insert(user_id) {
            let st = self.clone();
            tokio::spawn(async move {
                let p = match st.discord.member(guild_id, user_id).await {
                    Ok(Some(p)) => Some(p),
                    _ => st.discord.user(user_id).await,
                };
                if let Some(p) = p {
                    st.profiles.insert(user_id, p);
                }
                st.profile_pending.remove(&user_id);
            });
        }
        None
    }

    /// Profile now (cache, then Discord), for short lists like the DJs.
    pub async fn profile(&self, guild_id: u64, user_id: u64) -> Option<Profile> {
        if let Some(p) = self.profiles.get(&user_id) {
            return Some(p);
        }
        let p = match self.discord.member(guild_id, user_id).await {
            Ok(Some(p)) => Some(p),
            _ => self.discord.user(user_id).await,
        }?;
        self.profiles.insert(user_id, p.clone());
        Some(p)
    }
}

/// Socket address of the client (absent in tests that call the router directly).
pub struct PeerAddr(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0)))
    }
}

/// A valid session (any route except `/api/public` and `/api/auth`).
pub struct Auth(pub Arc<Session>);

fn bearer(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

impl FromRequestParts<ApiState> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &ApiState) -> Result<Self, Self::Rejection> {
        let token = bearer(parts).ok_or_else(ApiError::unauthorized)?;
        st.sessions
            .get(token)
            .await
            .map(Auth)
            .ok_or_else(ApiError::unauthorized)
    }
}

/// A session in a guild whose user is still a member (re-checked, cached for `MEMBER_TTL`).
pub struct Member {
    pub session: Arc<Session>,
    pub guild_id: u64,
    pub status: MemberStatus,
}

impl Member {
    pub fn user_id(&self) -> u64 {
        self.session.user_id
    }
}

impl FromRequestParts<ApiState> for Member {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &ApiState) -> Result<Self, Self::Rejection> {
        let Auth(session) = Auth::from_request_parts(parts, st).await?;
        let guild_id = session
            .guild_id
            .ok_or_else(|| ApiError::invalid("Open the panel from a server"))?;
        let status = st.member_status(guild_id, session.user_id).await?;
        if !status.member {
            return Err(ApiError::no_permission());
        }
        Ok(Self {
            session,
            guild_id,
            status,
        })
    }
}

#[cfg(test)]
mod client_ip_tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn uses_x_real_ip_never_x_forwarded_for() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        // A client-supplied first hop cannot pick its rate-limit key.
        let h = headers(&[("x-forwarded-for", "1.2.3.4, 9.9.9.9"), ("x-real-ip", "9.9.9.9")]);
        assert_eq!(client_ip(&h, Some(peer)), "9.9.9.9");
        assert_eq!(client_ip(&headers(&[("x-forwarded-for", "1.2.3.4")]), Some(peer)), "127.0.0.1");
        assert_eq!(client_ip(&headers(&[("x-real-ip", "not an ip")]), Some(peer)), "127.0.0.1");
        assert_eq!(client_ip(&HeaderMap::new(), None), "unknown");
    }

    #[test]
    fn ipv6_counts_per_64() {
        let a = client_ip(&headers(&[("x-real-ip", "2001:db8:1:2:aaaa::1")]), None);
        let b = client_ip(&headers(&[("x-real-ip", "2001:db8:1:2:bbbb::2")]), None);
        assert_eq!(a, b);
        assert_eq!(a, "2001:db8:1:2::/64");
        assert_eq!(client_ip(&headers(&[("x-real-ip", "::ffff:1.2.3.4")]), None), "1.2.3.4");
    }
}
