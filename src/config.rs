//! Runtime configuration, read once from environment variables (`.env` or Docker `environment`).

use std::{env, path::PathBuf, str::FromStr};

use anyhow::{bail, Context, Result};

/// `PREFETCH_TRACKS` when unset, and its upper bound.
pub const DEFAULT_PREFETCH_TRACKS: usize = 2;
pub const MAX_PREFETCH_TRACKS: usize = 5;

#[derive(Debug, Clone)]
pub struct Config {
    /// Main bot: owns the slash commands, the Activity and every text message.
    pub discord_token: String,
    pub discord_client_id: String,
    pub discord_client_secret: String,
    /// Extra voice-only bots, used when the main bot is busy in another voice channel of the same guild.
    pub node_tokens: Vec<String>,

    pub public_url: String,
    /// Internal HTTP server (nginx in the image proxies `/backend/` to it).
    pub http_host: String,
    pub http_port: u16,
    /// Persistent app data: SQLite database, updated yt-dlp binary.
    pub appdata_dir: PathBuf,
    /// Heavy data: stream fragments per voice channel, large caches.
    pub data_dir: PathBuf,
    pub bot_locale: String,
    /// Activity shown on every Lynx bot once startup finished (`BOT_ACTIVITY`, empty: none).
    pub bot_activity: Option<String>,
    /// `custom` (default), `playing`, `listening`, `watching` or `competing`.
    pub bot_activity_type: String,

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
    /// Acceptable alternates to `ollama_model` (the GPU may be shared): if one of them is already
    /// loaded it is used instead of loading the preferred model.
    pub ollama_checked_models: Vec<String>,
    /// Wait before retrying a model load that failed (usually a busy GPU).
    pub ollama_load_retry_secs: u64,

    pub ytdlp_cookies_file: Option<PathBuf>,
    /// `--limit-rate` for the yt-dlp piped fallback (HLS/live), like PIXL's 300K. Empty = no limit.
    pub ytdlp_limit_rate: Option<String>,
    /// YouTube player client yt-dlp tries first for stream URLs (`YTDLP_PLAYER_CLIENT`, default
    /// `visionos`: no start-up wait). Empty or `default`: yt-dlp's own client choice only.
    pub ytdlp_player_client: Option<String>,
    /// Extractors yt-dlp may use (`YTDLP_EXTRACTORS`, its `--use-extractors` syntax). `None`: the
    /// built-in allowlist (YouTube, SoundCloud, Bandcamp, Vimeo...), never the generic extractor.
    pub ytdlp_extractors: Option<String>,
    /// Sites whose links users may play (`LINK_HOSTS`, comma-separated domains, subdomains
    /// included; `*` = any public host). Empty: the built-in list matching `ytdlp_extractors`.
    pub link_hosts: Vec<String>,
    pub empty_channel_timeout_secs: u64,
    pub max_concurrent_extractions: usize,
    pub stream_cache_max_mb: u64,
    /// Upcoming tracks each player prepares ahead (stream URL plus the first ~10 s of audio on
    /// disk), so skipping several tracks still starts at once. 0 turns prefetching off.
    pub prefetch_tracks: usize,
    pub cache_ttl_days: u32,
    /// Saved playlists per user (Playlists tab). When full, the least recently used unpinned one
    /// is dropped. 0 turns saving off.
    pub max_stored_playlists: usize,

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
            http_host: optional("HTTP_HOST").unwrap_or_else(|| "127.0.0.1".into()),
            http_port: parse_or("HTTP_PORT", 4000)?,
            appdata_dir: optional("APPDATA_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("./appdata")),
            data_dir: optional("DATA_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("./data")),
            bot_locale: optional("BOT_LOCALE").unwrap_or_else(|| "en".into()),
            bot_activity: Some(optional("BOT_ACTIVITY").unwrap_or_else(|| "/play".into())).filter(|a| a != "-"),
            bot_activity_type: optional("BOT_ACTIVITY_TYPE")
                .map(|t| t.to_ascii_lowercase())
                .unwrap_or_else(|| "custom".into()),

            spotify_client_id: optional("SPOTIFY_CLIENT_ID"),
            spotify_client_secret: optional("SPOTIFY_CLIENT_SECRET"),
            lastfm_api_key: optional("LASTFM_API_KEY"),
            musicbrainz_user_agent: optional("MUSICBRAINZ_USER_AGENT"),

            ai_enabled: flag("AI_ENABLED", false)?,
            ollama_url: optional("OLLAMA_URL").map(|u| u.trim_end_matches('/').to_owned()),
            ollama_model: optional("OLLAMA_MODEL"),
            ollama_keep_alive: optional("OLLAMA_KEEP_ALIVE").unwrap_or_else(|| "30m".into()),
            ollama_timeout_secs: parse_or("OLLAMA_TIMEOUT_SECS", 120)?,
            ollama_checked_models: list("OLLAMA_CHECKED_MODELS"),
            ollama_load_retry_secs: parse_or("OLLAMA_LOAD_RETRY_SECS", 30)?,

            ytdlp_cookies_file: optional("YTDLP_COOKIES_FILE").map(PathBuf::from),
            ytdlp_limit_rate: Some(optional("YTDLP_LIMIT_RATE").unwrap_or_else(|| "300K".into())),
            ytdlp_player_client: Some(
                optional("YTDLP_PLAYER_CLIENT")
                    .unwrap_or_else(|| crate::sources::ytdlp::DEFAULT_PLAYER_CLIENT.into()),
            ),
            ytdlp_extractors: optional("YTDLP_EXTRACTORS"),
            link_hosts: list("LINK_HOSTS"),
            empty_channel_timeout_secs: parse_or("EMPTY_CHANNEL_TIMEOUT_SECS", 180)?,
            max_concurrent_extractions: parse_or("MAX_CONCURRENT_EXTRACTIONS", 8)?,
            stream_cache_max_mb: parse_or("STREAM_CACHE_MAX_MB", 2048)?,
            prefetch_tracks: parse_or::<usize>("PREFETCH_TRACKS", DEFAULT_PREFETCH_TRACKS)?.min(MAX_PREFETCH_TRACKS),
            cache_ttl_days: parse_or("CACHE_TTL_DAYS", 7)?,
            max_stored_playlists: parse_or("MAX_STORED_PLAYLISTS", 20)?,

            landing_url: optional("LANDING_URL"),
            terms_url: optional("TERMS_URL"),
            privacy_url: optional("PRIVACY_URL"),
            support_url: optional("SUPPORT_URL"),
        };

        if cfg.spotify_client_id.is_some() != cfg.spotify_client_secret.is_some() {
            bail!("SPOTIFY_CLIENT_ID and SPOTIFY_CLIENT_SECRET must be set together");
        }
        if !matches!(
            cfg.bot_activity_type.as_str(),
            "custom" | "playing" | "listening" | "watching" | "competing"
        ) {
            bail!("invalid BOT_ACTIVITY_TYPE={}: expected custom, playing, listening, watching or competing", cfg.bot_activity_type);
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
        self.appdata_dir.join("bin")
    }

    /// yt-dlp's cache (YouTube player JS, challenge solutions), kept across restarts.
    pub fn ytdlp_cache_dir(&self) -> PathBuf {
        self.appdata_dir.join("cache").join("yt-dlp")
    }

        pub fn database_path(&self) -> PathBuf {
        self.appdata_dir.join("lynx.db")
    }
}

pub(crate) fn optional(key: &str) -> Option<String> {
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

pub(crate) fn flag(key: &str, default: bool) -> Result<bool> {
    match optional(key).map(|v| v.to_ascii_lowercase()) {
        None => Ok(default),
        Some(v) if matches!(v.as_str(), "true" | "1" | "yes" | "on") => Ok(true),
        Some(v) if matches!(v.as_str(), "false" | "0" | "no" | "off") => Ok(false),
        Some(v) => bail!("invalid {key}={v}: expected true or false"),
    }
}

pub(crate) fn parse_or<T: FromStr>(key: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match optional(key) {
        None => Ok(default),
        Some(raw) => raw.parse().map_err(|e| anyhow::anyhow!("invalid {key}={raw}: {e}")),
    }
}
