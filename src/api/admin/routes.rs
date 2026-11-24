//! Admin API handlers. Everything here needs a signed-in admin ([`Admin`]).
//!
//! Runtime tweaks (no restart, nothing that reads secrets or runs arbitrary commands):
//! - maintenance mode: refuse new songs (`/play`, `/ai`, panel adds) with the notice text;
//!   what is playing keeps playing;
//! - pause the daily yt-dlp self-update;
//! - check for a yt-dlp update now (the same GitHub release check the daily task does);
//! - clear the in-memory stream URL cache;
//! - clear the SQLite lookup cache (searches, lists, YouTube matches).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

use super::{auth::Admin, AdminState, YtdlpJob};
use crate::{
    api::{
        error::{ApiError, ApiResult, Body},
        health,
    },
    discord::about,
    notices::{Flags, Kind, NewNotice, MAX_MESSAGE, MAX_TITLE},
    store::now_secs,
};

/// Notices kept at most (old ones are pruned automatically).
const MAX_NOTICES: i64 = 100;
/// Longest notice: 7 days.
const MAX_MINUTES: i64 = 7 * 24 * 60;
/// Accepted time range (2020-01-01 .. 2100-01-01), in epoch seconds.
const MIN_TIME: i64 = 1_577_836_800;
const MAX_TIME: i64 = 4_102_444_800;

fn internal(e: anyhow::Error) -> ApiError {
    warn!(error = %e, "admin request failed");
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "Something went wrong")
}

/// `GET /api/overview`: the `/health` report plus memory, flags and notice counts.
pub async fn overview(_: Admin, State(st): State<AdminState>) -> ApiResult<Json<Value>> {
    let report = health::compute(&st.app, &st.health_limits).await;
    let now = now_secs();
    let notices = st.app.notices.notices();
    let visible = crate::notices::visible(&notices, now).len();
    Ok(Json(json!({
        "health": report,
        "memoryBytes": about::memory_bytes(),
        "flags": st.app.notices.flags(),
        "notices": { "total": notices.len(), "visible": visible },
        "serverNow": now * 1000,
    })))
}

fn notices_json(st: &AdminState) -> Value {
    let now = now_secs();
    let list: Vec<Value> = st.app.notices.notices().iter().map(|n| n.admin_json(now)).collect();
    json!({ "notices": list, "serverNow": now * 1000 })
}

/// `GET /api/notices`: every stored notice, soonest first.
pub async fn list_notices(_: Admin, State(st): State<AdminState>) -> Json<Value> {
    Json(notices_json(&st))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NoticeBody {
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    message: String,
    /// Epoch ms (UTC).
    starts_at: i64,
    /// Either a duration or an end time.
    #[serde(default)]
    duration_minutes: Option<i64>,
    #[serde(default)]
    ends_at: Option<i64>,
    /// When users start seeing it; default now.
    #[serde(default)]
    show_from: Option<i64>,
    #[serde(default = "yes")]
    active: bool,
}

fn yes() -> bool {
    true
}

/// Check a notice from the admin UI. `now` in epoch seconds.
pub fn validate(b: NoticeBody, now: i64) -> ApiResult<NewNotice> {
    let kind =
        Kind::parse(b.kind.trim()).ok_or_else(|| ApiError::invalid("kind must be maintenance, info or warning"))?;
    let title = b.title.trim().to_owned();
    let message = b.message.trim().to_owned();
    if title.chars().count() > MAX_TITLE {
        return Err(ApiError::invalid(format!(
            "the title is longer than {MAX_TITLE} characters"
        )));
    }
    if message.chars().count() > MAX_MESSAGE {
        return Err(ApiError::invalid(format!(
            "the message is longer than {MAX_MESSAGE} characters"
        )));
    }
    if title.is_empty() && message.is_empty() {
        return Err(ApiError::invalid("add a title or a message"));
    }
    let secs = |ms: i64| ms.div_euclid(1000);
    let in_range = |t: i64| (MIN_TIME..MAX_TIME).contains(&t);
    let starts_at = secs(b.starts_at);
    if !in_range(starts_at) {
        return Err(ApiError::invalid("the start time is out of range"));
    }
    let ends_at = match (b.ends_at, b.duration_minutes) {
        (Some(end), _) => secs(end),
        (None, Some(m)) if (1..=MAX_MINUTES).contains(&m) => starts_at + m * 60,
        (None, Some(_)) => return Err(ApiError::invalid("the duration must be 1 minute to 7 days")),
        (None, None) => return Err(ApiError::invalid("give a duration or an end time")),
    };
    if ends_at <= starts_at || ends_at - starts_at > MAX_MINUTES * 60 {
        return Err(ApiError::invalid(
            "the end must be after the start, at most 7 days later",
        ));
    }
    let show_from = b.show_from.map(secs).unwrap_or(now);
    if !in_range(show_from) {
        return Err(ApiError::invalid("the show from time is out of range"));
    }
    if show_from >= ends_at {
        return Err(ApiError::invalid("show from must be before the end"));
    }
    Ok(NewNotice {
        kind,
        title,
        message,
        starts_at,
        ends_at,
        show_from,
        active: b.active,
    })
}

async fn reload(st: &AdminState) -> ApiResult<()> {
    let list = st.app.store.notices().await.map_err(internal)?;
    st.app.notices.set_notices(list);
    Ok(())
}

/// `POST /api/notices`
pub async fn create_notice(
    _: Admin,
    State(st): State<AdminState>,
    Body(b): Body<NoticeBody>,
) -> ApiResult<Json<Value>> {
    let now = now_secs();
    let n = validate(b, now)?;
    if n.ends_at <= now {
        return Err(ApiError::invalid("that notice has already ended"));
    }
    if st.app.store.notice_count().await.map_err(internal)? >= MAX_NOTICES {
        return Err(ApiError::invalid("too many notices, delete some first"));
    }
    let id = st.app.store.insert_notice(&n).await.map_err(internal)?;
    info!(
        id,
        kind = n.kind.as_str(),
        starts_at = n.starts_at,
        "admin created a notice"
    );
    reload(&st).await?;
    Ok(Json(notices_json(&st)))
}

fn notice_id(raw: &str) -> ApiResult<i64> {
    raw.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| ApiError::not_found("no such notice"))
}

/// `PUT /api/notices/{id}`
pub async fn update_notice(
    _: Admin,
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Body(b): Body<NoticeBody>,
) -> ApiResult<Json<Value>> {
    let id = notice_id(&id)?;
    let n = validate(b, now_secs())?;
    if !st.app.store.update_notice(id, &n).await.map_err(internal)? {
        return Err(ApiError::not_found("no such notice"));
    }
    info!(id, kind = n.kind.as_str(), active = n.active, "admin edited a notice");
    reload(&st).await?;
    Ok(Json(notices_json(&st)))
}

/// `DELETE /api/notices/{id}`
pub async fn delete_notice(_: Admin, State(st): State<AdminState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let id = notice_id(&id)?;
    if !st.app.store.delete_notice(id).await.map_err(internal)? {
        return Err(ApiError::not_found("no such notice"));
    }
    info!(id, "admin deleted a notice");
    reload(&st).await?;
    Ok(Json(notices_json(&st)))
}

fn tweaks_json(st: &AdminState) -> Value {
    let job = st.ytdlp_job.lock().unwrap_or_else(|p| p.into_inner()).clone();
    json!({
        "flags": st.app.notices.flags(),
        "ytdlp": {
            "version": health::ytdlp_report(st.app.ytdlp_version.line()).version,
            "job": job,
        },
    })
}

/// `GET /api/tweaks`
pub async fn get_tweaks(_: Admin, State(st): State<AdminState>) -> Json<Value> {
    Json(tweaks_json(&st))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TweaksBody {
    #[serde(default)]
    maintenance_mode: Option<bool>,
    #[serde(default)]
    ytdlp_updates_paused: Option<bool>,
}

/// `PUT /api/tweaks`: change one or more runtime flags (saved, kept across restarts).
pub async fn put_tweaks(_: Admin, State(st): State<AdminState>, Body(b): Body<TweaksBody>) -> ApiResult<Json<Value>> {
    let old = st.app.notices.flags();
    let new = Flags {
        maintenance_mode: b.maintenance_mode.unwrap_or(old.maintenance_mode),
        ytdlp_updates_paused: b.ytdlp_updates_paused.unwrap_or(old.ytdlp_updates_paused),
    };
    st.app.store.set_admin_flags(&new).await.map_err(internal)?;
    st.app.notices.set_flags(new);
    st.app.sources.ytdlp().set_updates_paused(new.ytdlp_updates_paused);
    info!(
        maintenance_mode = new.maintenance_mode,
        ytdlp_updates_paused = new.ytdlp_updates_paused,
        "admin changed the runtime flags"
    );
    Ok(Json(tweaks_json(&st)))
}

/// `POST /api/actions/ytdlp-update`: start a yt-dlp update check in the background (one at a time).
pub async fn ytdlp_update(_: Admin, State(st): State<AdminState>) -> ApiResult<(StatusCode, Json<Value>)> {
    {
        let mut job = st.ytdlp_job.lock().unwrap_or_else(|p| p.into_inner());
        if job.running {
            return Ok((StatusCode::ACCEPTED, Json(tweaks_json(&st))));
        }
        job.running = true;
    }
    info!("admin started a yt-dlp update check");
    let bg = st.clone();
    tokio::spawn(async move {
        let ytdlp = bg.app.sources.ytdlp().clone();
        let outcome = ytdlp.update().await;
        let version = ytdlp.version().await;
        let (outcome, label) = match outcome {
            Ok(Some(v)) => {
                info!(version = %v, "yt-dlp updated from the admin panel");
                ("updated", Some(v))
            }
            Ok(None) => ("up_to_date", version.clone()),
            Err(e) => {
                warn!(error = %e, "yt-dlp update check from the admin panel failed");
                ("failed", version.clone())
            }
        };
        bg.app.ytdlp_version.set(version);
        let mut job = bg.ytdlp_job.lock().unwrap_or_else(|p| p.into_inner());
        *job = YtdlpJob {
            running: false,
            finished_at: Some(now_secs() * 1000),
            outcome: Some(outcome),
            version: label.and_then(|v| health::ytdlp_report(about::YtdlpLine::Known(v)).version),
        };
    });
    Ok((StatusCode::ACCEPTED, Json(tweaks_json(&st))))
}

/// `POST /api/actions/clear-stream-cache`
pub async fn clear_stream_cache(_: Admin, State(st): State<AdminState>) -> Json<Value> {
    let removed = st.app.sources.ytdlp().clear_stream_cache();
    info!(removed, "admin cleared the stream URL cache");
    Json(json!({ "removed": removed }))
}

/// `POST /api/actions/clear-lookup-cache`
pub async fn clear_lookup_cache(_: Admin, State(st): State<AdminState>) -> ApiResult<Json<Value>> {
    let removed = st.app.store.cache_clear().await.map_err(internal)?;
    info!(removed, "admin cleared the lookup cache");
    Ok(Json(json!({ "removed": removed })))
}
