//! HTTP server for the Activity panel: REST API, WebSocket events and the media proxies.
//!
//! nginx serves the built panel at `/` and proxies `/backend/*` here with the prefix stripped, so
//! this router only defines `/api/...`, `/ws`, `/cdn/...`, `/img` and `/healthz` (plan/api.md).
//! `/health` (public report for uptime monitors) is also proxied at the domain root.

mod ai;
mod auth;
mod error;
mod follow;
pub(crate) mod health;
mod jam;
mod me;
mod media;
mod notices;
mod player;
mod playlists;
mod queue;
mod search;
mod seek_hold;
pub(crate) mod session;
mod settings;
mod upstream;
mod ws;

#[cfg(test)]
mod tests;

use std::{
    net::SocketAddr,
    ops::Deref,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use anyhow::Result;
use axum::{
    routing::{delete, get, post},
    Router,
};
use tokio::sync::broadcast;
use tracing::info;

use crate::AppState;
use session::{RateLimiter, Sessions, TtlCache};
use upstream::{Discord, LiveDiscord, Profile};

const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_SESSIONS: usize = 50_000;
/// Membership and manager status are re-checked through Discord at most this often per user.
const MEMBER_TTL: Duration = Duration::from_secs(30);
const PROFILE_TTL: Duration = Duration::from_secs(60 * 60);
const GENRE_TTL: Duration = Duration::from_secs(120);
const SWEEP_EVERY: Duration = Duration::from_secs(5 * 60);

/// Membership of a user in a guild, as last checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberStatus {
    pub member: bool,
    pub manager: bool,
}

/// Guild-level events fanned out to the open sockets.
#[derive(Debug, Clone)]
pub enum GuildEvent {
    /// The guild and its settings as `GET /api/settings` returns them.
    Settings(u64, serde_json::Value),
    /// Something only one user sees changed (their votes, their DJ role): resend state and permissions.
    User(u64),
    /// Operator notices changed (every socket): the ready `notices` message.
    Notices(serde_json::Value),
}

pub struct Options {
    pub session_ttl: Duration,
    /// Thresholds of the `/health` status rules.
    pub health_limits: health::Limits,
    /// Longest seek-bar hold (`seek_hold::HOLD_MAX`; tests use a short one).
    pub seek_hold_max: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            session_ttl: SESSION_TTL,
            health_limits: health::LIMITS,
            seek_hold_max: seek_hold::HOLD_MAX,
        }
    }
}

pub struct Inner {
    pub app: AppState,
    pub discord: Arc<dyn Discord>,
    pub sessions: Sessions,
    pub auth_limit: RateLimiter<String>,
    /// Shared with the Discord commands (`AppState::limits`).
    pub search_limit: Arc<RateLimiter<u64>>,
    pub add_limit: Arc<RateLimiter<u64>>,
    pub ai_limit: Arc<RateLimiter<u64>>,
    /// Saved-playlist refreshes (they reach Spotify / YouTube): see `playlists`.
    pub refresh_limit: playlists::RefreshLimiter,
    pub members: TtlCache<(u64, u64), MemberStatus>,
    pub profiles: TtlCache<u64, Profile>,
    /// Track key -> genres (cached tags, short TTL so new tags show up soon).
    pub genres: TtlCache<String, Vec<String>>,
    /// Profiles being fetched in the background (deduplication).
    pub profile_pending: dashmap::DashSet<u64>,
    /// Open WebSockets per user (cap, and closing them on sign-out).
    pub sockets: ws::Sockets,
    pub events: broadcast::Sender<GuildEvent>,
    pub media: reqwest::Client,
    pub search_refused_logged: AtomicBool,
    /// Last `/health` report (see `health::CACHE_TTL`).
    pub health: health::HealthCache<health::HealthReport>,
    pub health_limits: health::Limits,
    /// Who drags the progress bar, per player channel / jam.
    pub seek_holds: seek_hold::SeekHolds,
}

/// Router state. Cheap to clone.
#[derive(Clone)]
pub struct ApiState(Arc<Inner>);

impl Deref for ApiState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl ApiState {
    pub fn new(app: AppState, discord: Arc<dyn Discord>, opts: Options) -> Result<Self> {
        let media = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(crate::sources::USER_AGENT)
            .build()?;
        let store = app.store.clone();
        let limits = app.limits.clone();
        Ok(Self(Arc::new(Inner {
            app,
            discord,
            sessions: Sessions::new(store, opts.session_ttl, MAX_SESSIONS),
            auth_limit: RateLimiter::new(10, Duration::from_secs(60), 20_000),
            search_limit: limits.search.clone(),
            add_limit: limits.add.clone(),
            ai_limit: limits.ai.clone(),
            refresh_limit: playlists::RefreshLimiter::new(20_000),
            members: TtlCache::new(MEMBER_TTL, 50_000),
            profiles: TtlCache::new(PROFILE_TTL, 20_000),
            genres: TtlCache::new(GENRE_TTL, 20_000),
            profile_pending: dashmap::DashSet::new(),
            sockets: ws::Sockets::default(),
            events: broadcast::channel(256).0,
            media,
            search_refused_logged: AtomicBool::new(false),
            health: health::HealthCache::new(health::CACHE_TTL),
            health_limits: opts.health_limits,
            seek_holds: seek_hold::SeekHolds::new(opts.seek_hold_max, seek_hold::REACQUIRE_COOLDOWN),
        })))
    }

    fn sweep(&self) {
        let n = self.sessions.sweep();
        self.auth_limit.sweep();
        self.app.limits.sweep();
        self.refresh_limit.sweep();
        self.members.sweep();
        self.profiles.sweep();
        self.genres.sweep();
        if n > 0 {
            tracing::debug!(expired = n, "panel sessions expired");
        }
    }
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/health", get(health::health))
        .route("/health/", get(health::health))
        .route("/health/bots/{index}", get(health::bot))
        .route("/api/public", get(auth::public))
        .route("/api/notices", get(notices::list))
        .route("/api/auth", post(auth::login).delete(auth::logout))
        .route("/api/player", get(player::state))
        .route("/api/player/pause", post(player::pause))
        .route("/api/player/resume", post(player::resume))
        .route("/api/player/skip", post(player::skip))
        .route("/api/player/previous", post(player::previous))
        .route("/api/player/seek", post(player::seek))
        .route("/api/player/volume", post(player::volume))
        .route("/api/player/loop", post(player::set_loop))
        .route("/api/player/shuffle", post(player::shuffle))
        .route("/api/player/stop", post(player::stop))
        .route("/api/queue/add", post(queue::add))
        .route("/api/queue/move", post(queue::move_track))
        .route("/api/queue/remove", post(queue::remove))
        .route("/api/queue/clear", post(queue::clear))
        .route("/api/tracks/feedback", post(queue::feedback))
        .route("/api/playlists", get(playlists::list))
        .route("/api/playlists/save", post(playlists::save))
        .route("/api/playlists/{id}", delete(playlists::remove))
        .route("/api/playlists/{id}/pin", post(playlists::pin))
        .route("/api/playlists/{id}/tracks", get(playlists::tracks))
        .route("/api/playlists/{id}/refresh", post(playlists::refresh))
        .route("/api/playlists/{id}/add", post(playlists::add))
        .route("/api/ai/start", post(ai::start))
        .route("/api/ai/stop", post(ai::stop))
        .route("/api/ai/cancel", post(ai::cancel))
        .route("/api/jam", get(jam::get))
        .route("/api/jam/create", post(jam::create))
        .route("/api/jam/join", post(jam::join))
        .route("/api/jam/leave", post(jam::leave))
        .route("/api/jam/settings", post(jam::settings))
        .route("/api/jam/kick", post(jam::kick))
        .route("/api/jam/close", post(jam::close))
        .route("/api/jam/end", post(jam::end))
        .route("/api/jam/{id}", get(jam::preview))
        .route("/api/search", get(search::search))
        .route("/api/lyrics", get(search::lyrics))
        .route("/api/settings", get(settings::get_settings).put(settings::put_settings))
        .route("/api/djs", get(settings::list_djs).post(settings::add_dj))
        .route("/api/djs/{user_id}", delete(settings::remove_dj))
        .route("/api/members/candidates", get(settings::candidates))
        .route("/api/guild/channels", get(settings::channels))
        .route("/api/me/prefs", get(me::get_prefs).put(me::put_prefs))
        .route("/api/me/taste", get(me::taste))
        .route("/api/me/taste/genre/{genre}", get(me::genre_tracks))
        .route("/api/me/history/{track_key}", delete(me::forget_track))
        .route("/api/me/data", delete(me::delete_data))
        .route("/api/me/channel", get(follow::me_channel))
        .route("/ws", get(ws::upgrade))
        .route("/cdn/{*path}", get(media::cdn))
        .route("/img", get(media::img))
        // Activity counters for the admin panel (matched routes only, so the groups stay bounded).
        .route_layer(axum::middleware::from_fn(count_request))
        .fallback(|| async { error::ApiError::not_found("no such route") })
        .with_state(state)
}

async fn count_request(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    if let Some(p) = req.extensions().get::<axum::extract::MatchedPath>() {
        crate::stats::api(crate::stats::api_group(p.as_str()));
    }
    next.run(req).await
}

pub async fn serve(state: AppState) -> Result<()> {
    let discord = Arc::new(LiveDiscord::new(&state.config, state.bots.clone())?);
    let api = ApiState::new(state.clone(), discord, Options::default())?;
    {
        let api = api.clone();
        let stop = state.shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_EVERY);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tick.tick() => api.sweep(),
                }
            }
        });
    }
    tokio::spawn(notices::forward(api.clone(), state.shutdown.clone()));
    let app = router(api);
    let addr: SocketAddr = format!("{}:{}", state.config.http_host, state.config.http_port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "http listening");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move { state.shutdown.cancelled().await })
        .await?;
    Ok(())
}
