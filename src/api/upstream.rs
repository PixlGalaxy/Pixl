//! Everything the API asks Discord: the OAuth code exchange, `users/@me`, guild membership through
//! the main bot, and lookups in the main bot's cache. Behind a trait so tests never hit Discord.

use std::{sync::Arc, time::Duration};

use serde::Deserialize;
use serenity::{
    all::{ChannelId, ChannelType, GuildId, UserId},
    async_trait,
};

use crate::{
    config::Config,
    discord::{registry::BotRegistry, respond},
    player::messages::discord_error_code,
};

const DISCORD_API: &str = "https://discord.com/api/v10";
const TIMEOUT: Duration = Duration::from_secs(10);
/// Discord error codes for "this member / user does not exist".
const UNKNOWN_MEMBER: isize = 10007;
const UNKNOWN_USER: isize = 10013;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthUser {
    pub id: u64,
    pub username: String,
    pub global_name: Option<String>,
    /// Raw Discord CDN URL.
    pub avatar_url: Option<String>,
    pub locale: Option<String>,
}

/// Display name and avatar (raw Discord CDN URL) of a guild member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildSummary {
    pub name: String,
    pub icon_url: Option<String>,
}

/// Channel kinds the restriction pickers use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// Text and announcement channels.
    Text,
    /// Voice and stage channels (their chat accepts commands too).
    Voice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildChannel {
    pub id: u64,
    pub name: String,
    pub kind: ChannelKind,
    pub parent_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpstreamError {
    /// Discord refused the code or token (used twice, expired, wrong secret).
    #[error("rejected by Discord")]
    Rejected,
    /// Network problem, timeout or an unexpected answer.
    #[error("{0}")]
    Unavailable(String),
}

#[async_trait]
pub trait Discord: Send + Sync + 'static {
    /// OAuth2 authorization code -> user access token.
    async fn exchange_code(&self, code: &str) -> Result<String, UpstreamError>;
    async fn current_user(&self, access_token: &str) -> Result<OAuthUser, UpstreamError>;
    /// The member through the main bot's REST; `Ok(None)` when the user is not in the guild.
    async fn member(&self, guild_id: u64, user_id: u64) -> Result<Option<Profile>, UpstreamError>;
    /// Owner, Administrator or Manage Server.
    async fn is_manager(&self, guild_id: u64, user_id: u64) -> bool;
    /// Member search by name prefix; `None` when Discord refuses it.
    async fn search_members(&self, guild_id: u64, query: &str, limit: u32) -> Option<Vec<(u64, Profile)>>;
    /// Name of a user outside any guild context (requesters restored without a name).
    async fn user(&self, user_id: u64) -> Option<Profile>;

    fn guild(&self, guild_id: u64) -> Option<GuildSummary>;
    /// Name of a channel of that guild (None when it is not one).
    fn channel_name(&self, guild_id: u64, channel_id: u64) -> Option<String>;
    fn user_voice_channel(&self, guild_id: u64, user_id: u64) -> Option<u64>;
    /// Humans in any voice channel of the guild, with the profile when the cache has it.
    fn voice_members(&self, guild_id: u64) -> Vec<(u64, Option<Profile>)>;
    fn bot_name(&self, bot_index: usize) -> String;
    /// Text and voice channels the main bot can see, in the order Discord shows them.
    fn guild_channels(&self, guild_id: u64) -> Vec<GuildChannel>;
}

/// The real thing: reqwest for OAuth, the main bot's HTTP client and cache for the rest.
pub struct LiveDiscord {
    bots: Arc<BotRegistry>,
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
}

impl LiveDiscord {
    pub fn new(config: &Config, bots: Arc<BotRegistry>) -> anyhow::Result<Self> {
        Ok(Self {
            bots,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(TIMEOUT)
                .user_agent(crate::sources::USER_AGENT)
                .build()?,
            client_id: config.discord_client_id.clone(),
            client_secret: config.discord_client_secret.clone(),
        })
    }
}

fn unavailable(e: impl std::fmt::Display) -> UpstreamError {
    UpstreamError::Unavailable(e.to_string())
}

fn profile_of(m: &serenity::all::Member) -> Profile {
    Profile {
        name: m.display_name().to_owned(),
        avatar_url: Some(m.face()),
    }
}

/// CDN URL of a user avatar (or the default one).
pub fn avatar_url(user_id: u64, avatar: Option<&str>) -> String {
    match avatar {
        Some(hash) => {
            let ext = if hash.starts_with("a_") { "gif" } else { "png" };
            format!("https://cdn.discordapp.com/avatars/{user_id}/{hash}.{ext}?size=128")
        }
        None => format!("https://cdn.discordapp.com/embed/avatars/{}.png", (user_id >> 22) % 6),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct MeResponse {
    id: String,
    username: String,
    global_name: Option<String>,
    avatar: Option<String>,
    locale: Option<String>,
}

#[async_trait]
impl Discord for LiveDiscord {
    async fn exchange_code(&self, code: &str) -> Result<String, UpstreamError> {
        let res = self
            .http
            .post(format!("{DISCORD_API}/oauth2/token"))
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("grant_type", "authorization_code"),
                ("code", code),
            ])
            .send()
            .await
            .map_err(|e| unavailable(e.without_url()))?;
        if res.status().is_client_error() {
            return Err(UpstreamError::Rejected);
        }
        if !res.status().is_success() {
            return Err(unavailable(format!("token exchange returned {}", res.status())));
        }
        let body: TokenResponse = res.json().await.map_err(|e| unavailable(e.without_url()))?;
        Ok(body.access_token)
    }

    async fn current_user(&self, access_token: &str) -> Result<OAuthUser, UpstreamError> {
        let res = self
            .http
            .get(format!("{DISCORD_API}/users/@me"))
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| unavailable(e.without_url()))?;
        if res.status().is_client_error() {
            return Err(UpstreamError::Rejected);
        }
        if !res.status().is_success() {
            return Err(unavailable(format!("users/@me returned {}", res.status())));
        }
        let me: MeResponse = res.json().await.map_err(|e| unavailable(e.without_url()))?;
        let id: u64 = me.id.parse().map_err(|_| unavailable("bad user id"))?;
        Ok(OAuthUser {
            id,
            avatar_url: Some(avatar_url(id, me.avatar.as_deref())),
            username: me.username,
            global_name: me.global_name,
            locale: me.locale,
        })
    }

    async fn member(&self, guild_id: u64, user_id: u64) -> Result<Option<Profile>, UpstreamError> {
        let main = self.bots.main();
        match tokio::time::timeout(
            TIMEOUT,
            main.http.get_member(GuildId::new(guild_id), UserId::new(user_id)),
        )
        .await
        {
            Ok(Ok(m)) => Ok(Some(profile_of(&m))),
            Ok(Err(e)) if matches!(discord_error_code(&e), Some(UNKNOWN_MEMBER | UNKNOWN_USER)) => Ok(None),
            // Not in the guild at all (Missing Access / Unknown Guild): nobody is a member for us.
            Ok(Err(_)) if !self.bots.main_in_guild(guild_id) => Ok(None),
            Ok(Err(e)) => Err(unavailable(e)),
            Err(_) => Err(unavailable("timed out")),
        }
    }

    async fn is_manager(&self, guild_id: u64, user_id: u64) -> bool {
        respond::is_manager(&self.bots, guild_id, user_id, None).await
    }

    async fn search_members(&self, guild_id: u64, query: &str, limit: u32) -> Option<Vec<(u64, Profile)>> {
        let main = self.bots.main();
        let res = tokio::time::timeout(
            TIMEOUT,
            main.http
                .search_guild_members(GuildId::new(guild_id), query, Some(u64::from(limit.clamp(1, 100)))),
        )
        .await;
        match res {
            Ok(Ok(list)) => Some(
                list.iter()
                    .filter(|m| !m.user.bot)
                    .map(|m| (m.user.id.get(), profile_of(m)))
                    .collect(),
            ),
            _ => None,
        }
    }

    async fn user(&self, user_id: u64) -> Option<Profile> {
        let main = self.bots.main();
        let u = tokio::time::timeout(TIMEOUT, main.http.get_user(UserId::new(user_id)))
            .await
            .ok()?
            .ok()?;
        Some(Profile {
            name: u.display_name().to_owned(),
            avatar_url: Some(u.face()),
        })
    }

    fn guild(&self, guild_id: u64) -> Option<GuildSummary> {
        let g = self.bots.main().cache.guild(GuildId::new(guild_id))?;
        Some(GuildSummary {
            name: g.name.clone(),
            icon_url: g.icon_url(),
        })
    }

    fn channel_name(&self, guild_id: u64, channel_id: u64) -> Option<String> {
        let g = self.bots.main().cache.guild(GuildId::new(guild_id))?;
        g.channels.get(&ChannelId::new(channel_id)).map(|c| c.name.clone())
    }

    fn user_voice_channel(&self, guild_id: u64, user_id: u64) -> Option<u64> {
        self.bots.user_voice_channel(guild_id, user_id)
    }

    fn voice_members(&self, guild_id: u64) -> Vec<(u64, Option<Profile>)> {
        let Some(g) = self.bots.main().cache.guild(GuildId::new(guild_id)) else {
            return Vec::new();
        };
        g.voice_states
            .values()
            .filter(|vs| vs.channel_id.is_some())
            .filter_map(|vs| {
                let member = vs.member.as_ref().or_else(|| g.members.get(&vs.user_id));
                if member.is_some_and(|m| m.user.bot) || self.bots.is_lynx_bot(vs.user_id.get()) {
                    return None;
                }
                Some((vs.user_id.get(), member.map(profile_of)))
            })
            .collect()
    }

    fn guild_channels(&self, guild_id: u64) -> Vec<GuildChannel> {
        let main = self.bots.main();
        let Some(g) = main.cache.guild(GuildId::new(guild_id)) else {
            return Vec::new();
        };
        let me = g.members.get(&UserId::new(main.user_id().max(1)));
        let mut list: Vec<(i64, u16, u64, GuildChannel)> = g
            .channels
            .values()
            .filter_map(|c| {
                let kind = match c.kind {
                    ChannelType::Text | ChannelType::News => ChannelKind::Text,
                    ChannelType::Voice | ChannelType::Stage => ChannelKind::Voice,
                    _ => return None,
                };
                if let Some(me) = me {
                    let p = g.user_permissions_in(c, me);
                    if !p.administrator() && !p.view_channel() {
                        return None;
                    }
                }
                let parent = c.parent_id.and_then(|id| g.channels.get(&id));
                Some((
                    parent.map(|p| i64::from(p.position)).unwrap_or(-1),
                    c.position,
                    c.id.get(),
                    GuildChannel {
                        id: c.id.get(),
                        name: c.name.clone(),
                        kind,
                        parent_name: parent.map(|p| p.name.clone()),
                    },
                ))
            })
            .collect();
        list.sort_by_key(|(pp, p, id, _)| (*pp, *p, *id));
        list.into_iter().map(|(_, _, _, c)| c).collect()
    }

    fn bot_name(&self, bot_index: usize) -> String {
        self.bots
            .bot(bot_index)
            .map(|b| b.name().to_owned())
            .unwrap_or_else(|| "Lynx".into())
    }
}
