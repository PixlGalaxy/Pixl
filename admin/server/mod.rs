//! Operator admin panel backend: status dashboard, activity summary, live logs, notices /
//! scheduled downtime and a few safe runtime tweaks. The UI lives in `admin/web`.
//!
//! Isolation: the admin listener runs on its OWN small tokio runtime (2 worker threads) on a
//! dedicated OS thread, so a flood or a hang there cannot take worker time from the bot. Requests
//! have a timeout, a body limit and a concurrency limit, connections are capped, and a panic is
//! turned into a 500 (and, if the server itself dies, it is logged and restarted with a backoff;
//! the bot never notices). Handlers read cheap snapshots (the status snapshot computed on the bot's
//! runtime, the atomic activity counters, the log ring); anything that touches the database or
//! yt-dlp is sent to the bot's runtime and awaited with a short timeout.
//!
//! Security: off unless both `ADMIN_USERNAME` and `ADMIN_PASSWORD` (or `ADMIN_PASSWORD_HASH`,
//! Argon2) are set. Constant-time compares of both fields, per-IP and global sign-in limits, a
//! random session token in an HttpOnly + SameSite=Strict cookie (Secure over HTTPS) that expires,
//! a CSRF guard on every mutating request. The listener binds 127.0.0.1:4001 by default; in the
//! image only nginx's 8081 server proxies to it, never the Activity (8080).

mod auth;
mod error;
mod limit;
mod logs;
mod routes;
mod snapshot;
#[cfg(test)]
mod tests;

use std::{net::SocketAddr, panic::AssertUnwindSafe, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    extract::DefaultBodyLimit,
    http::{header, HeaderValue},
    middleware,
    routing::{any, get, post, put},
    Router,
};
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use crate::{
    config::{flag, optional, parse_or},
    AppState,
};
use auth::{AdminSessions, Credentials};
use error::AdminError;
use crate::api::session::RateLimiter;
use limit::LimitedListener;
pub use snapshot::Snapshot;

/// Shortest `ADMIN_PASSWORD` accepted; a shorter one keeps the admin panel off.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// Sign-in attempts per client address per minute, and for everyone together.
pub const LOGIN_PER_IP: u32 = 5;
pub const LOGIN_GLOBAL: u32 = 30;
const MAX_SESSIONS: usize = 20;
/// Hardening of the admin listener.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BODY: usize = 64 * 1024;
const MAX_IN_FLIGHT: usize = 32;
const MAX_CONNECTIONS: usize = 64;
const MAX_LOG_STREAMS: usize = 4;
/// Work sent to the bot's runtime (database, caches) gives up after this.
pub const MAIN_TIMEOUT: Duration = Duration::from_secs(8);
const SWEEP_EVERY: Duration = Duration::from_secs(5 * 60);
const WORKER_THREADS: usize = 2;
const RESTART_MAX: Duration = Duration::from_secs(60);

/// `ADMIN_*` environment variables.
#[derive(Clone)]
pub struct AdminConfig {
    /// None: the admin panel is off.
    pub credentials: Option<Credentials>,
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
            .field("credentials", &self.credentials.as_ref().map(|_| "<set>"))
            .field("host", &self.host)
            .field("port", &self.port)
            .field("web_dir", &self.web_dir)
            .field("cookie_secure", &self.cookie_secure)
            .finish()
    }
}

impl AdminConfig {
    pub fn from_env() -> Result<Self> {
        let credentials = Credentials::from_parts(
            optional("ADMIN_USERNAME"),
            optional("ADMIN_PASSWORD"),
            optional("ADMIN_PASSWORD_HASH"),
        );
        let cookie_secure = match optional("ADMIN_COOKIE_SECURE").map(|v| v.to_ascii_lowercase()) {
            None => None,
            Some(v) if v == "auto" => None,
            Some(_) => Some(flag("ADMIN_COOKIE_SECURE", false)?),
        };
        Ok(Self {
            credentials,
            host: optional("ADMIN_HTTP_HOST").unwrap_or_else(|| "127.0.0.1".into()),
            port: parse_or("ADMIN_HTTP_PORT", 4001)?,
            web_dir: optional("ADMIN_WEB_DIR").map(PathBuf::from),
            cookie_secure,
        })
    }

    pub fn enabled(&self) -> bool {
        self.credentials.is_some()
    }
}

/// What the admin backend gets from the bot.
#[derive(Clone)]
pub struct AdminDeps {
    pub app: AppState,
    /// The bot's runtime: database work, cache clears and yt-dlp checks run there.
    pub main: tokio::runtime::Handle,
    /// Latest status (bots, services, totals), computed on the bot's runtime.
    pub snapshot: tokio::sync::watch::Receiver<Option<Arc<Snapshot>>>,
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
    pub deps: AdminDeps,
    credentials: Credentials,
    sessions: AdminSessions,
    login_ip: RateLimiter<String>,
    login_all: RateLimiter<()>,
    cookie_secure: Option<bool>,
    pub ytdlp_job: Arc<std::sync::Mutex<YtdlpJob>>,
    log_streams: Arc<Semaphore>,
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
    /// None when the admin panel is off (no credentials).
    pub fn new(deps: AdminDeps, cfg: &AdminConfig) -> Option<Self> {
        let credentials = cfg.credentials.clone()?;
        Some(Self(Arc::new(AdminInner {
            deps,
            credentials,
            sessions: AdminSessions::new(SESSION_TTL, MAX_SESSIONS),
            login_ip: RateLimiter::new(LOGIN_PER_IP, Duration::from_secs(60), 10_000),
            login_all: RateLimiter::new(LOGIN_GLOBAL, Duration::from_secs(60), 1),
            cookie_secure: cfg.cookie_secure,
            ytdlp_job: Default::default(),
            log_streams: Arc::new(Semaphore::new(MAX_LOG_STREAMS)),
        })))
    }

    pub fn app(&self) -> &AppState {
        &self.deps.app
    }

    /// Run `fut` on the bot's runtime and wait for it at most [`MAIN_TIMEOUT`].
    pub async fn on_main<T, F>(&self, fut: F) -> Result<T, AdminError>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let task = self.deps.main.spawn(fut);
        match tokio::time::timeout(MAIN_TIMEOUT, task).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                warn!(error = %e, "admin job on the bot runtime failed");
                Err(AdminError::internal())
            }
            Err(_) => Err(AdminError::busy()),
        }
    }

    fn sweep(&self) {
        self.sessions.sweep();
        self.login_ip.sweep();
        self.login_all.sweep();
    }
}

const CSP: &str = "default-src 'self'; script-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; font-src 'self' data:; \
                   connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

fn not_found() -> AdminError {
    AdminError::not_found("no such route")
}

/// The admin API (and, with `web_dir`, the built admin UI), hardened.
pub fn router(state: AdminState, web_dir: Option<PathBuf>) -> Router {
    let api = Router::new()
        .route("/api/session", get(auth::session))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/overview", get(routes::overview))
        .route("/api/activity", get(routes::activity))
        .route("/api/logs", get(logs::recent))
        .route("/api/logs/stream", get(logs::stream))
        .route("/api/notices", get(routes::list_notices).post(routes::create_notice))
        .route(
            "/api/notices/{id}",
            put(routes::update_notice).delete(routes::delete_notice),
        )
        .route(
            "/api/auto-update",
            get(routes::get_auto_update).put(routes::put_auto_update),
        )
        .route("/api/auto-update/check", post(routes::check_auto_update))
        .route("/api/tweaks", get(routes::get_tweaks).put(routes::put_tweaks))
        .route("/api/actions/ytdlp-update", post(routes::ytdlp_update))
        .route("/api/actions/clear-stream-cache", post(routes::clear_stream_cache))
        .route("/api/actions/clear-lookup-cache", post(routes::clear_lookup_cache))
        .route("/api/{*rest}", any(|| async { not_found() }))
        .route("/api", any(|| async { not_found() }))
        .layer(middleware::from_fn(no_store));
    let app = match web_dir {
        Some(dir) => {
            let index = dir.join("index.html");
            api.fallback_service(
                tower_http::services::ServeDir::new(dir)
                    .append_index_html_on_directories(false)
                    .fallback(tower_http::services::ServeFile::new(index)),
            )
        }
        None => api.fallback(|| async { not_found() }),
    };
    harden(
        app.layer(middleware::from_fn(auth::csrf_guard))
            .layer(middleware::from_fn(security_headers))
            .with_state(state),
        REQUEST_TIMEOUT,
    )
}

/// Timeout (408), body size limit, in-flight request limit and panic catching (500) around any
/// admin router. A panicking or hanging handler never reaches the bot.
pub fn harden(router: Router, timeout: Duration) -> Router {
    router
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            timeout,
        ))
        .layer(tower::limit::ConcurrencyLimitLayer::new(MAX_IN_FLIGHT))
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
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

/// Start the admin panel when it is configured: the status snapshot task on the bot's runtime
/// (call this from it), and the listener on its own thread and runtime. Never fails the bot.
pub fn start(app: AppState, cfg: AdminConfig) -> Option<std::thread::JoinHandle<()>> {
    if !cfg.enabled() {
        return None;
    }
    let main = tokio::runtime::Handle::current();
    let snapshot = snapshot::spawn(app.clone(), app.shutdown.clone());
    let deps = AdminDeps { app, main, snapshot };
    let state = AdminState::new(deps, &cfg)?;
    let spawned = std::thread::Builder::new()
        .name("lynx-admin".into())
        .spawn(move || run_isolated(state, cfg));
    match spawned {
        Ok(h) => Some(h),
        Err(e) => {
            error!(error = %e, "could not start the admin thread; the admin panel is unavailable");
            None
        }
    }
}

/// The admin listener's own runtime, restarted with a backoff when it fails or panics.
fn run_isolated(state: AdminState, cfg: AdminConfig) {
    let stop = state.app().shutdown.clone();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .thread_name("lynx-admin-rt")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "could not build the admin runtime; the admin panel is unavailable");
            return;
        }
    };
    {
        let state = state.clone();
        let stop = stop.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_EVERY);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tick.tick() => state.sweep(),
                }
            }
        });
    }
    let mut backoff = Duration::from_secs(1);
    loop {
        if stop.is_cancelled() {
            break;
        }
        let run = std::panic::catch_unwind(AssertUnwindSafe(|| rt.block_on(serve(state.clone(), &cfg))));
        match run {
            Ok(Ok(())) if stop.is_cancelled() => break,
            Ok(Ok(())) => warn!("admin listener stopped; restarting"),
            Ok(Err(e)) => {
                error!(error = %e, retry_secs = backoff.as_secs(), "admin listener failed; the bot keeps running")
            }
            Err(_) => error!(
                retry_secs = backoff.as_secs(),
                "admin listener panicked; the bot keeps running"
            ),
        }
        let stop2 = stop.clone();
        rt.block_on(async move {
            tokio::select! {
                _ = stop2.cancelled() => {}
                _ = tokio::time::sleep(backoff) => {}
            }
        });
        backoff = (backoff * 2).min(RESTART_MAX);
    }
    rt.shutdown_timeout(Duration::from_secs(2));
}

async fn serve(state: AdminState, cfg: &AdminConfig) -> Result<()> {
    let stop = state.app().shutdown.clone();
    let router = router(state, cfg.web_dir.clone());
    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse()?;
    let listener = LimitedListener::new(tokio::net::TcpListener::bind(addr).await?, MAX_CONNECTIONS);
    info!(%addr, ui = cfg.web_dir.is_some(), "admin http listening");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<limit::PeerAddr>(),
    )
    .with_graceful_shutdown(async move { stop.cancelled().await })
    .await?;
    Ok(())
}

/// `lynx hash-admin-password`: read a password from stdin, print its Argon2id hash for
/// `ADMIN_PASSWORD_HASH`.
pub fn hash_password_cli() -> Result<()> {
    let mut line = String::new();
    eprintln!("Password, then Enter:");
    std::io::stdin().read_line(&mut line)?;
    let password = line.trim_end_matches(['\r', '\n']);
    anyhow::ensure!(!password.is_empty(), "the password must not be empty");
    println!("{}", auth::hash_password(password)?);
    Ok(())
}
