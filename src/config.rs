//! Runtime configuration, read once from environment variables (`.env` or Docker `environment`).

use std::{env, path::PathBuf, str::FromStr};

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    /// Main bot: owns the slash commands, the Activity and every text message.
    pub discord_token: String,
    pub discord_client_id: String,
    pub discord_client_secret: String,
    /// Extra voice-only bots, used when the main bot is busy in another voice channel of the same guild.
    pub node_tokens: Vec<String>,

    pub public_url: String,
    pub http_port: u16,
    pub data_dir: PathBuf,
    pub bot_locale: String,

    pub spotify_client_id: Option<String>,
    pub spotify_client_secret: Option<String>,
    pub lastfm_api_key: Option<String>,
    pub musicbrainz_user_agent: Option<String>,

    /// AI features (/ai radio, Like/Dislike, listening history). Ollama is optional on top of this:
    /// without it the radio uses only the recommendation algorithm.
    pub ai_enabled: bool,
    pub ollama_url: Option<String>,
    pub ollama_model: Option<String>,
    pub ollama_keep_alive: String,
    pub ollama_timeout_secs: u64,

    pub ytdlp_cookies_file: Option<PathBuf>,
    pub empty_channel_timeout_secs: u64,
    pub max_concurrent_extractions: usize,
    pub stream_cache_max_mb: u64,
    pub cache_ttl_days: u32,

    pub landing_url: Option<String>,
    pub terms_url: Option<String>,
    pub privacy_url: Option<String>,
    pub support_url: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let cfg = Self {
            discord_token: required("DISCORD_TOKEN")?,
            discord_client_id: required("DISCORD_CLIENT_ID")?,
            discord_client_secret: required("DISCORD_CLIENT_SECRET")?,
            node_tokens: list("NODE_TOKENS"),

            public_url: required("PUBLIC_URL")?.trim_end_matches('/').to_owned(),
            http_port: parse_or("HTTP_PORT", 8080)?,
            data_dir: optional("DATA_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("./data")),
            bot_locale: optional("BOT_LOCALE").unwrap_or_else(|| "en".into()),

            spotify_client_id: optional("SPOTIFY_CLIENT_ID"),
            spotify_client_secret: optional("SPOTIFY_CLIENT_SECRET"),
            lastfm_api_key: optional("LASTFM_API_KEY"),
            musicbrainz_user_agent: optional("MUSICBRAINZ_USER_AGENT"),

            ai_enabled: flag("AI_ENABLED", false)?,
            ollama_url: optional("OLLAMA_URL").map(|u| u.trim_end_matches('/').to_owned()),
            ollama_model: optional("OLLAMA_MODEL"),
            ollama_keep_alive: optional("OLLAMA_KEEP_ALIVE").unwrap_or_else(|| "30m".into()),
            ollama_timeout_secs: parse_or("OLLAMA_TIMEOUT_SECS", 120)?,

            ytdlp_cookies_file: optional("YTDLP_COOKIES_FILE").map(PathBuf::from),
            empty_channel_timeout_secs: parse_or("EMPTY_CHANNEL_TIMEOUT_SECS", 180)?,
            max_concurrent_extractions: parse_or("MAX_CONCURRENT_EXTRACTIONS", 8)?,
            stream_cache_max_mb: parse_or("STREAM_CACHE_MAX_MB", 2048)?,
            cache_ttl_days: parse_or("CACHE_TTL_DAYS", 7)?,

            landing_url: optional("LANDING_URL"),
            terms_url: optional("TERMS_URL"),
            privacy_url: optional("PRIVACY_URL"),
            support_url: optional("SUPPORT_URL"),
        };

        if cfg.spotify_client_id.is_some() != cfg.spotify_client_secret.is_some() {
            bail!("SPOTIFY_CLIENT_ID and SPOTIFY_CLIENT_SECRET must be set together");
        }
        if cfg.ollama_url.is_some() && cfg.ollama_model.is_none() {
            bail!("OLLAMA_MODEL is required when OLLAMA_URL is set");
        }
        Ok(cfg)
    }

    pub fn streams_dir(&self) -> PathBuf {
        self.data_dir.join("streams")
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.data_dir.join("bin")
    }

    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("lynx.db")
    }
}

fn optional(key: &str) -> Option<String> {
    env::var(key).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

fn required(key: &str) -> Result<String> {
    optional(key).with_context(|| format!("missing required environment variable {key}"))
}

fn list(key: &str) -> Vec<String> {
    optional(key)
        .map(|v| v.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

fn flag(key: &str, default: bool) -> Result<bool> {
    match optional(key).map(|v| v.to_ascii_lowercase()) {
        None => Ok(default),
        Some(v) if matches!(v.as_str(), "true" | "1" | "yes" | "on") => Ok(true),
        Some(v) if matches!(v.as_str(), "false" | "0" | "no" | "off") => Ok(false),
        Some(v) => bail!("invalid {key}={v}: expected true or false"),
    }
}

fn parse_or<T: FromStr>(key: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match optional(key) {
        None => Ok(default),
        Some(raw) => raw.parse().map_err(|e| anyhow::anyhow!("invalid {key}={raw}: {e}")),
    }
}
