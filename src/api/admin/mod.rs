//! Operator admin panel: notices / scheduled downtime, an overview (the `/health` report) and a few
//! safe runtime tweaks.
//!
//! It runs on its OWN listener (`ADMIN_HTTP_HOST:ADMIN_HTTP_PORT`, 127.0.0.1:4001 by default),
//! never on the Activity one: in the image nginx serves the admin UI on port 8081 and proxies only
//! that server's `/api/` here, while the 8080 server (Activity) has no route to it. The whole
//! thing is off unless `ADMIN_PASSWORD` is set (at least [`MIN_PASSWORD_CHARS`] characters).
//!
//! Security: password compared in constant time (SHA-256 digests), per-IP and global login rate
//! limits, a random session token in an HttpOnly + SameSite=Strict cookie (Secure over HTTPS) that
//! expires, and a CSRF guard on every mutating request (custom header + same-origin check). Nothing
//! here reads secrets, files or runs commands; see `routes` for the full list of actions.

mod auth;
mod routes;
#[cfg(test)]
mod tests;

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    http::{header, HeaderValue},
    middleware,
    routing::{any, get, post, put},
    Router,
};
use tracing::{info, warn};

use super::{error::ApiError, health, session::RateLimiter};
use crate::{
    config::{flag, optional, parse_or},
    AppState,
};
use auth::AdminSessions;

/// Shortest `ADMIN_PASSWORD` accepted; a shorter one keeps the admin panel off.
pub const MIN_PASSWORD_CHARS: usize = 12;
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// Login attempts per client address per minute, and for everyone together.
pub const LOGIN_PER_IP: u32 = 5;
pub const LOGIN_GLOBAL: u32 = 30;
const MAX_SESSIONS: usize = 20;
const SWEEP_EVERY: Duration = Duration::from_secs(5 * 60);

/// `ADMIN_*` environment variables.
#[derive(Clone)]
pub struct AdminConfig {
    pub password: Option<String>,
    pub host: String,
    pub port: u16,
    /// Built admin UI to serve from this listener (Windows dev run without nginx).
    pub web_dir: Option<PathBuf>,
    /// `ADMIN_COOKIE_SECURE`: None = auto (Secure when the request came over HTTPS).
    pub cookie_secure: Option<bool>,
}

impl std::fmt::Debug for AdminConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminConfig")
            .field("password", &self.password.as_ref().map(|_| "<set>"))
            .field("host", &self.host)
            .field("port", &self.port)
            .field("web_dir", &self.web_dir)
            .field("cookie_secure", &self.cookie_secure)
            .finish()
    }
}

impl AdminConfig {
    pub fn from_env() -> Result<Self> {
        let mut password = optional("ADMIN_PASSWORD");
        if password
            .as_ref()
            .is_some_and(|p| p.chars().count() < MIN_PASSWORD_CHARS)
        {
            warn!(
                min = MIN_PASSWORD_CHARS,
                "ADMIN_PASSWORD is too short; the admin panel stays off"
            );
            password = None;
        }
        let cookie_secure = match optional("ADMIN_COOKIE_SECURE").map(|v| v.to_ascii_lowercase()) {
            None => None,
            Some(v) if v == "auto" => None,
            Some(_) => Some(flag("ADMIN_COOKIE_SECURE", false)?),
        };
        Ok(Self {
            password,
            host: optional("ADMIN_HTTP_HOST").unwrap_or_else(|| "127.0.0.1".into()),
            port: parse_or("ADMIN_HTTP_PORT", 4001)?,
            web_dir: optional("ADMIN_WEB_DIR").map(PathBuf::from),
            cookie_secure,
        })
    }

    pub fn enabled(&self) -> bool {
        self.password.is_some()
    }
}

/// Last manual yt-dlp update check (Tweaks page).
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct YtdlpJob {
    pub running: bool,
    /// Epoch ms of the last finished check.
    pub finished_at: Option<i64>,
    /// `updated`, `up_to_date` or `failed` (details only in the log).
    pub outcome: Option<&'static str>,
    pub version: Option<String>,
}

pub struct AdminInner {
    pub app: AppState,
    password_digest: [u8; 32],
    sessions: AdminSessions,
    login_ip: RateLimiter<String>,
    login_all: RateLimiter<()>,
    cookie_secure: Option<bool>,
    pub ytdlp_job: std::sync::Mutex<YtdlpJob>,
    pub health_limits: health::Limits,
}

/// Router state of the admin listener. Cheap to clone.
#[derive(Clone)]
pub struct AdminState(Arc<AdminInner>);

impl std::ops::Deref for AdminState {
    type Target = AdminInner;
    fn deref(&self) -> &AdminInner {
        &self.0
    }
}

impl AdminState {
    /// None when the admin panel is off (no password).
    pub fn new(app: AppState, cfg: &AdminConfig) -> Option<Self> {
        let password = cfg.password.as_deref()?;
        Some(Self(Arc::new(AdminInner {
            app,
            password_digest: auth::digest(password),
            sessions: AdminSessions::new(SESSION_TTL, MAX_SESSIONS),
            login_ip: RateLimiter::new(LOGIN_PER_IP, Duration::from_secs(60), 10_000),
            login_all: RateLimiter::new(LOGIN_GLOBAL, Duration::from_secs(60), 1),
            cookie_secure: cfg.cookie_secure,
            ytdlp_job: Default::default(),
            health_limits: health::LIMITS,
        })))
    }

    fn sweep(&self) {
        self.sessions.sweep();
        self.login_ip.sweep();
        self.login_all.sweep();
    }
}

const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; font-src 'self' data:; \
                   connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// The admin API (and, with `web_dir`, the built admin UI).
pub fn router(state: AdminState, web_dir: Option<PathBuf>) -> Router {
    let api = Router::new()
        .route("/api/session", get(auth::session))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/overview", get(routes::overview))
        .route("/api/notices", get(routes::list_notices).post(routes::create_notice))
        .route(
            "/api/notices/{id}",
            put(routes::update_notice).delete(routes::delete_notice),
        )
        .route("/api/tweaks", get(routes::get_tweaks).put(routes::put_tweaks))
        .route("/api/actions/ytdlp-update", post(routes::ytdlp_update))
        .route("/api/actions/clear-stream-cache", post(routes::clear_stream_cache))
        .route("/api/actions/clear-lookup-cache", post(routes::clear_lookup_cache))
        .route("/api/{*rest}", any(|| async { ApiError::not_found("no such route") }))
        .route("/api", any(|| async { ApiError::not_found("no such route") }))
        .layer(middleware::from_fn(no_store));
    let app = match web_dir {
        Some(dir) => {
            let index = dir.join("admin.html");
            api.fallback_service(
                tower_http::services::ServeDir::new(dir)
                    .append_index_html_on_directories(false)
                    .fallback(tower_http::services::ServeFile::new(index)),
            )
        }
        None => api.fallback(|| async { ApiError::not_found("no such route") }),
    };
    app.layer(middleware::from_fn(auth::csrf_guard))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// API answers are never cached.
async fn no_store(req: axum::extract::Request, next: middleware::Next) -> axum::response::Response {
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

async fn security_headers(req: axum::extract::Request, next: middleware::Next) -> axum::response::Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("x-robots-tag", HeaderValue::from_static("noindex, nofollow"));
    h.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    res
}

pub async fn serve(app: AppState, cfg: AdminConfig) -> Result<()> {
    let Some(state) = AdminState::new(app.clone(), &cfg) else {
        return Ok(());
    };
    {
        let state = state.clone();
        let stop = app.shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_EVERY);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tick.tick() => state.sweep(),
                }
            }
        });
    }
    let router = router(state, cfg.web_dir.clone());
    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, ui = cfg.web_dir.is_some(), "admin http listening");
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move { app.shutdown.cancelled().await })
        .await?;
    Ok(())
}
