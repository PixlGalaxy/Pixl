//! Public health report for uptime monitors (Better Uptime / Better Stack).
//!
//! `GET /health` returns the whole report: HTTP 200 for `ok` and `degraded`, 503 for `down`.
//! `GET /health/bots/{index}` returns one bot: 200 while it is connected (or within the reconnect
//! grace), 503 when it is down, 404 for an unknown index. nginx exposes both at the Activity
//! domain root without the panel's frame_id gate (see `nginx.conf` and the README "Monitoring").
//!
//! The report is cached for [`CACHE_TTL`] so a flood of requests never reaches the database or
//! Ollama; it carries no tokens, ids, guild or channel names, user data, paths or error strings.

use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Path, State},
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use super::{error::ApiError, ApiState};
use crate::{
    discord::{
        about::{self, YtdlpLine},
        registry::GatewayStatus,
    },
    player::state::Status as PlayerStatus,
    AppState,
};

/// How long a computed report is served to every caller.
pub const CACHE_TTL: Duration = Duration::from_secs(5);
/// The database check gives up after this.
const DB_TIMEOUT: Duration = Duration::from_secs(2);
/// An Ollama `/api/ps` answer younger than this (from the radio or the tagger) is reused.
const OLLAMA_MAX_AGE: Duration = Duration::from_secs(60);

/// Thresholds of the status rules.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// A gateway outage shorter than this is a normal reconnect and does not count yet.
    pub grace: Duration,
    /// Heartbeat latency above this is degraded.
    pub max_ping: Duration,
}

pub const LIMITS: Limits = Limits {
    grace: Duration::from_secs(30),
    max_ping: Duration::from_millis(1000),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Degraded,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Main,
    Voice,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Shards {
    pub total: u32,
    pub connected: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BotReport {
    pub index: usize,
    pub role: Role,
    /// Display name of the bot.
    pub name: String,
    pub status: Status,
    /// Every shard has a live gateway session right now.
    pub connected: bool,
    pub shards: Shards,
    pub gateway_ping_ms: Option<u64>,
    pub guilds: usize,
    pub voice_sessions: usize,
    /// Start of the current outage (RFC 3339), null while fully connected.
    pub down_since: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseReport {
    pub ok: bool,
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct YtdlpReport {
    pub ok: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OllamaReport {
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Services {
    pub database: DatabaseReport,
    pub ytdlp: YtdlpReport,
    /// null when Ollama is not configured.
    pub ollama: Option<OllamaReport>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Players {
    pub active: usize,
    pub playing: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    pub status: Status,
    pub version: &'static str,
    pub commit: Option<&'static str>,
    pub uptime_secs: u64,
    pub checked_at: String,
    pub bots: Vec<BotReport>,
    pub services: Services,
    pub players: Players,
}

// ---- status rules -------------------------------------------------------------------------

/// One bot: down when no shard has been connected for longer than the grace; degraded when some
/// shard has been missing for longer than the grace or the heartbeat is slow; else ok.
pub fn bot_status(g: &GatewayStatus, ping: Option<Duration>, limits: &Limits) -> Status {
    if g.down_for.is_some_and(|d| d >= limits.grace) {
        return Status::Down;
    }
    if g.degraded_for.is_some_and(|d| d >= limits.grace) {
        return Status::Degraded;
    }
    if g.shards_connected > 0 && ping.is_some_and(|p| p > limits.max_ping) {
        return Status::Degraded;
    }
    Status::Ok
}

/// The whole report. `bots[0]` is the main bot.
pub fn overall(bots: &[Status], db_ok: bool, ytdlp_ok: bool, ollama_ok: Option<bool>) -> Status {
    if bots.first().is_none_or(|s| *s == Status::Down) || !db_ok {
        return Status::Down;
    }
    if bots.iter().any(|s| *s != Status::Ok) || !ytdlp_ok || ollama_ok == Some(false) {
        return Status::Degraded;
    }
    Status::Ok
}

/// A yt-dlp version is shown only when it looks like one (never an error text).
fn clean_version(v: &str) -> Option<String> {
    let v = v.trim();
    (!v.is_empty() && v.len() <= 32 && v.chars().all(|c| c.is_ascii_alphanumeric() || ".-+_".contains(c)))
        .then(|| v.to_owned())
}

pub fn ytdlp_report(line: YtdlpLine) -> YtdlpReport {
    match line {
        YtdlpLine::Known(v) => YtdlpReport {
            ok: true,
            version: clean_version(&v),
        },
        // Not asked yet (first seconds after start): not counted as a failure.
        YtdlpLine::Checking => YtdlpReport {
            ok: true,
            version: None,
        },
        YtdlpLine::Unavailable => YtdlpReport {
            ok: false,
            version: None,
        },
    }
}

/// `2026-11-24T12:34:56Z` (UTC, whole seconds).
pub fn rfc3339(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ---- computing the report -----------------------------------------------------------------

async fn check_database(app: &AppState) -> DatabaseReport {
    let started = Instant::now();
    let res = tokio::time::timeout(DB_TIMEOUT, sqlx::query("SELECT 1").execute(app.store.pool())).await;
    match res {
        Ok(Ok(_)) => DatabaseReport {
            ok: true,
            latency_ms: Some(started.elapsed().as_millis() as u64),
        },
        _ => DatabaseReport {
            ok: false,
            latency_ms: None,
        },
    }
}

pub async fn compute(app: &AppState, limits: &Limits) -> HealthReport {
    let players = app.players.all();
    let mut bots = Vec::with_capacity(app.bots.bots().len());
    for bot in app.bots.bots() {
        let gateway = bot.gateway_status().await;
        let ping = bot.gateway_latency().await;
        bots.push(BotReport {
            index: bot.index,
            role: if bot.is_main() { Role::Main } else { Role::Voice },
            name: bot.name().to_owned(),
            status: bot_status(&gateway, ping, limits),
            connected: gateway.shards_connected >= gateway.shards_total,
            shards: Shards {
                total: gateway.shards_total,
                connected: gateway.shards_connected,
            },
            gateway_ping_ms: ping
                .filter(|_| gateway.shards_connected > 0)
                .map(|p| p.as_millis() as u64),
            guilds: app.bots.guilds_of(bot.index).len(),
            voice_sessions: players.iter().filter(|p| p.bot_index == bot.index).count(),
            down_since: gateway.down_since.map(rfc3339),
        });
    }
    let (database, ollama) = tokio::join!(check_database(app), app.ai.ollama_reachable(OLLAMA_MAX_AGE));
    // Spawns a background check only when the cached answer is older than its TTL (15 min).
    app.ytdlp_version.refresh_if_stale(&app.sources);
    let ytdlp = ytdlp_report(app.ytdlp_version.line());
    let statuses: Vec<Status> = bots.iter().map(|b| b.status).collect();
    HealthReport {
        status: overall(&statuses, database.ok, ytdlp.ok, ollama),
        version: about::version(),
        commit: about::commit(),
        uptime_secs: about::uptime().as_secs(),
        checked_at: rfc3339(SystemTime::now()),
        bots,
        services: Services {
            database,
            ytdlp,
            ollama: ollama.map(|ok| OllamaReport { ok }),
        },
        players: Players {
            active: players.len(),
            playing: players
                .iter()
                .filter(|p| p.snapshot().status == PlayerStatus::Playing)
                .count(),
        },
    }
}

// ---- cache --------------------------------------------------------------------------------

/// Serves one computed value for `ttl`. The lock is held while computing, so concurrent callers
/// wait for that one computation instead of starting their own.
pub struct HealthCache<T> {
    ttl: Duration,
    slot: tokio::sync::Mutex<Option<(tokio::time::Instant, Arc<T>)>>,
}

impl<T> HealthCache<T> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            slot: tokio::sync::Mutex::new(None),
        }
    }

    pub async fn get_or_compute<F, Fut>(&self, compute: F) -> Arc<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let mut slot = self.slot.lock().await;
        if let Some((at, v)) = slot.as_ref() {
            if at.elapsed() < self.ttl {
                return v.clone();
            }
        }
        let v = Arc::new(compute().await);
        *slot = Some((tokio::time::Instant::now(), v.clone()));
        v
    }

    /// Drop the cached value (tests).
    #[cfg(test)]
    pub async fn invalidate(&self) {
        *self.slot.lock().await = None;
    }
}

// ---- handlers -----------------------------------------------------------------------------

fn with_headers(code: StatusCode, body: impl IntoResponse) -> Response {
    let mut res = (code, body).into_response();
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex"),
    );
    res
}

async fn report(s: &ApiState) -> Arc<HealthReport> {
    s.health.get_or_compute(|| compute(&s.app, &s.health_limits)).await
}

/// `GET /health`
pub async fn health(State(s): State<ApiState>) -> Response {
    let r = report(&s).await;
    let code = if r.status == Status::Down {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    with_headers(code, Json(r.as_ref().clone()))
}

/// `GET /health/bots/{index}`
pub async fn bot(State(s): State<ApiState>, Path(index): Path<String>) -> Response {
    let r = report(&s).await;
    let Some(bot) = index.parse::<usize>().ok().and_then(|i| r.bots.get(i)) else {
        let res = ApiError::not_found("no such bot").into_response();
        let (parts, body) = res.into_parts();
        return with_headers(parts.status, axum::body::Body::new(body));
    };
    let code = if bot.status == Status::Down {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    with_headers(code, Json(bot.clone()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn gw(total: u32, connected: u32, degraded_for: Option<u64>, down_for: Option<u64>) -> GatewayStatus {
        GatewayStatus {
            shards_total: total,
            shards_connected: connected,
            degraded_for: degraded_for.map(Duration::from_secs),
            down_for: down_for.map(Duration::from_secs),
            down_since: degraded_for.map(|_| UNIX_EPOCH),
        }
    }

    #[test]
    fn bot_status_rules() {
        let l = LIMITS;
        let ms = |n| Some(Duration::from_millis(n));
        assert_eq!(bot_status(&gw(1, 1, None, None), ms(40), &l), Status::Ok);
        // Fully down within the grace: a normal reconnect.
        assert_eq!(bot_status(&gw(1, 0, Some(10), Some(10)), None, &l), Status::Ok);
        // ...beyond it: down.
        assert_eq!(bot_status(&gw(1, 0, Some(31), Some(31)), None, &l), Status::Down);
        // One of two shards missing beyond the grace: degraded.
        assert_eq!(bot_status(&gw(2, 1, Some(45), None), ms(40), &l), Status::Degraded);
        assert_eq!(bot_status(&gw(2, 1, Some(5), None), ms(40), &l), Status::Ok);
        // Slow heartbeat.
        assert_eq!(bot_status(&gw(1, 1, None, None), ms(1500), &l), Status::Degraded);
        assert_eq!(bot_status(&gw(1, 1, None, None), ms(1000), &l), Status::Ok);
        // A stale ping of a disconnected bot does not matter within the grace.
        assert_eq!(bot_status(&gw(1, 0, Some(5), Some(5)), ms(5000), &l), Status::Ok);
    }

    #[test]
    fn overall_rules() {
        use Status::*;
        assert_eq!(overall(&[Ok, Ok], true, true, None), Ok);
        assert_eq!(overall(&[Ok, Ok], true, true, Some(true)), Ok);
        // Main bot down or database failing: down.
        assert_eq!(overall(&[Down, Ok], true, true, None), Down);
        assert_eq!(overall(&[Ok], false, true, None), Down);
        // A voice bot down, main partially down, yt-dlp or Ollama failing: degraded.
        assert_eq!(overall(&[Ok, Down], true, true, None), Degraded);
        assert_eq!(overall(&[Degraded, Ok], true, true, None), Degraded);
        assert_eq!(overall(&[Ok], true, false, None), Degraded);
        assert_eq!(overall(&[Ok], true, true, Some(false)), Degraded);
        assert_eq!(overall(&[], true, true, None), Down);
    }

    #[test]
    fn ytdlp_states() {
        let r = ytdlp_report(YtdlpLine::Known("2026.11.23".into()));
        assert!(r.ok);
        assert_eq!(r.version.as_deref(), Some("2026.11.23"));
        assert!(ytdlp_report(YtdlpLine::Checking).ok);
        assert!(!ytdlp_report(YtdlpLine::Unavailable).ok);
        // Never an error text or a path.
        let r = ytdlp_report(YtdlpLine::Known("/usr/bin/yt-dlp: error".into()));
        assert_eq!(r.version, None);
    }

    #[test]
    fn rfc3339_format() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_795_000_000)),
            "2026-11-18T11:06:40Z"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cache_serves_one_value_per_ttl() {
        let cache = HealthCache::new(CACHE_TTL);
        let calls = AtomicUsize::new(0);
        let get = || async {
            cache
                .get_or_compute(|| async { calls.fetch_add(1, Ordering::SeqCst) })
                .await
        };
        assert_eq!(*get().await, 0);
        assert_eq!(*get().await, 0);
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(*get().await, 0);
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(*get().await, 1);
        // Concurrent callers share one computation.
        tokio::time::advance(Duration::from_secs(6)).await;
        let (a, b) = tokio::join!(get(), get());
        assert_eq!((*a, *b), (2, 2));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
