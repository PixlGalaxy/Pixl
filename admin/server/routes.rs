//! Admin API handlers. Everything here needs a signed-in admin ([`Admin`]).
//!
//! Runtime tweaks (no restart, nothing that reads secrets or runs arbitrary commands):
//! - maintenance mode: refuse new songs (`/play`, `/ai`, panel adds) with the notice text;
//!   what is playing keeps playing;
//! - pause the daily yt-dlp self-update;
//! - check for a yt-dlp update now (the same GitHub release check the daily task does);
//! - clear the in-memory stream URL cache;
//! - clear the SQLite lookup cache (searches, lists, YouTube matches);
//! - automatic "scheduled update" notices: settings, status and a check now (`crate::updates`).
//!
//! Database and yt-dlp work runs on the bot's runtime ([`AdminState::on_main`]) with a timeout.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

use super::{
    auth::Admin,
    error::{AdminError, AdminResult, Body},
    snapshot, AdminState, YtdlpJob,
};
use crate::{
    api::health,
    discord::about,
    notices::{Flags, Kind, NewNotice, Notice, MAX_MESSAGE, MAX_TITLE},
    store::now_secs,
    updates::{self, Settings},
};

/// Notices kept at most (old ones are pruned automatically).
const MAX_NOTICES: i64 = 100;
/// Longest notice: 7 days.
const MAX_MINUTES: i64 = 7 * 24 * 60;
/// Accepted time range (2020-01-01 .. 2100-01-01), in epoch seconds.
const MIN_TIME: i64 = 1_577_836_800;
const MAX_TIME: i64 = 4_102_444_800;

fn db_error(e: anyhow::Error) -> AdminError {
    warn!(error = %e, "admin database request failed");
    AdminError::internal()
}

fn ytdlp_json(st: &AdminState) -> Value {
    let job = st.ytdlp_job.lock().unwrap_or_else(|p| p.into_inner()).clone();
    json!({
        "version": health::ytdlp_report(st.app().ytdlp_version.line()).version,
        "updatesPaused": st.app().notices.flags().ytdlp_updates_paused,
        "job": job,
    })
}

/// `GET /api/overview`: the latest status snapshot (bots, services, totals), flags and notices.
pub async fn overview(_: Admin, State(st): State<AdminState>) -> AdminResult<Json<Value>> {
    let latest = st.deps.snapshot.borrow().clone();
    let snap = match latest {
        Some(s) => s,
        // Just started: compute one now (on the bot's runtime).
        None => {
            let app = st.app().clone();
            std::sync::Arc::new(st.on_main(async move { snapshot::compute(&app).await }).await?)
        }
    };
    let now = now_secs();
    let notices = st.app().notices.notices();
    let visible = crate::notices::visible(&notices, now).len();
    Ok(Json(json!({
        "health": snap.health,
        "labels": snap.labels,
        "totals": snap.totals,
        "memoryBytes": snap.memory_bytes,
        "snapshotAt": snap.at,
        "flags": st.app().notices.flags(),
        "ytdlp": ytdlp_json(&st),
        "notices": { "total": notices.len(), "visible": visible },
        "serverNow": now * 1000,
    })))
}

/// `GET /api/activity`: counters for the last hour and the last 24 hours (lock-free reads).
pub async fn activity(_: Admin) -> Json<Value> {
    let mut v = crate::stats::summary();
    v["serverNow"] = json!(now_secs() * 1000);
    Json(v)
}

fn notices_json(list: &[Notice]) -> Value {
    let now = now_secs();
    let items: Vec<Value> = list.iter().map(|n| n.admin_json(now)).collect();
    json!({ "notices": items, "serverNow": now * 1000 })
}

/// `GET /api/notices`: every stored notice, soonest first.
pub async fn list_notices(_: Admin, State(st): State<AdminState>) -> Json<Value> {
    Json(notices_json(&st.app().notices.notices()))
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
pub fn validate(b: NoticeBody, now: i64) -> AdminResult<NewNotice> {
    let kind =
        Kind::parse(b.kind.trim()).ok_or_else(|| AdminError::invalid("kind must be maintenance, info or warning"))?;
    let title = b.title.trim().to_owned();
    let message = b.message.trim().to_owned();
    if title.chars().count() > MAX_TITLE {
        return Err(AdminError::invalid(format!(
            "the title is longer than {MAX_TITLE} characters"
        )));
    }
    if message.chars().count() > MAX_MESSAGE {
        return Err(AdminError::invalid(format!(
            "the message is longer than {MAX_MESSAGE} characters"
        )));
    }
    if title.is_empty() && message.is_empty() {
        return Err(AdminError::invalid("add a title or a message"));
    }
    let secs = |ms: i64| ms.div_euclid(1000);
    let in_range = |t: i64| (MIN_TIME..MAX_TIME).contains(&t);
    let starts_at = secs(b.starts_at);
    if !in_range(starts_at) {
        return Err(AdminError::invalid("the start time is out of range"));
    }
    let ends_at = match (b.ends_at, b.duration_minutes) {
        (Some(end), _) => secs(end),
        (None, Some(m)) if (1..=MAX_MINUTES).contains(&m) => starts_at + m * 60,
        (None, Some(_)) => return Err(AdminError::invalid("the duration must be 1 minute to 7 days")),
        (None, None) => return Err(AdminError::invalid("give a duration or an end time")),
    };
    if ends_at <= starts_at || ends_at - starts_at > MAX_MINUTES * 60 {
        return Err(AdminError::invalid(
            "the end must be after the start, at most 7 days later",
        ));
    }
    let show_from = b.show_from.map(secs).unwrap_or(now);
    if !in_range(show_from) {
        return Err(AdminError::invalid("the show from time is out of range"));
    }
    if show_from >= ends_at {
        return Err(AdminError::invalid("show from must be before the end"));
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

enum Change {
    Create(NewNotice),
    Update(i64, NewNotice),
    Delete(i64),
}

/// Apply a change on the bot's runtime and reload the board. Ok(false): no such notice.
async fn change(st: &AdminState, c: Change) -> AdminResult<Json<Value>> {
    let app = st.app().clone();
    let found = st
        .on_main(async move {
            let store = &app.store;
            let found = match &c {
                Change::Create(n) => {
                    if store.notice_count().await? >= MAX_NOTICES {
                        return Ok(None);
                    }
                    let id = store.insert_notice(n).await?;
                    info!(
                        id,
                        kind = n.kind.as_str(),
                        starts_at = n.starts_at,
                        "admin created a notice"
                    );
                    true
                }
                Change::Update(id, n) => {
                    let ok = store.update_notice(*id, n).await?;
                    if ok {
                        info!(id, kind = n.kind.as_str(), active = n.active, "admin edited a notice");
                    }
                    ok
                }
                Change::Delete(id) => {
                    let ok = store.delete_notice(*id).await?;
                    if ok {
                        info!(id, "admin deleted a notice");
                    }
                    ok
                }
            };
            app.notices.set_notices(store.notices().await?);
            anyhow::Ok(Some(found))
        })
        .await?
        .map_err(db_error)?;
    match found {
        None => Err(AdminError::invalid("too many notices, delete some first")),
        Some(false) => Err(AdminError::not_found("no such notice")),
        Some(true) => Ok(Json(notices_json(&st.app().notices.notices()))),
    }
}

/// `POST /api/notices`
pub async fn create_notice(
    _: Admin,
    State(st): State<AdminState>,
    Body(b): Body<NoticeBody>,
) -> AdminResult<Json<Value>> {
    let now = now_secs();
    let n = validate(b, now)?;
    if n.ends_at <= now {
        return Err(AdminError::invalid("that notice has already ended"));
    }
    change(&st, Change::Create(n)).await
}

fn notice_id(raw: &str) -> AdminResult<i64> {
    raw.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| AdminError::not_found("no such notice"))
}

/// `PUT /api/notices/{id}`
pub async fn update_notice(
    _: Admin,
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Body(b): Body<NoticeBody>,
) -> AdminResult<Json<Value>> {
    let id = notice_id(&id)?;
    let n = validate(b, now_secs())?;
    if st.app().notices.notices().iter().any(|n| n.id == id && n.auto) {
        return Err(AdminError::invalid(
            "this notice is managed by the automatic update notices; change their settings instead",
        ));
    }
    change(&st, Change::Update(id, n)).await
}

/// `DELETE /api/notices/{id}`
pub async fn delete_notice(_: Admin, State(st): State<AdminState>, Path(id): Path<String>) -> AdminResult<Json<Value>> {
    let id = notice_id(&id)?;
    change(&st, Change::Delete(id)).await
}

fn tweaks_json(st: &AdminState) -> Value {
    json!({ "flags": st.app().notices.flags(), "ytdlp": ytdlp_json(st) })
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
pub async fn put_tweaks(_: Admin, State(st): State<AdminState>, Body(b): Body<TweaksBody>) -> AdminResult<Json<Value>> {
    let app = st.app().clone();
    st.on_main(async move {
        let old = app.notices.flags();
        let new = Flags {
            maintenance_mode: b.maintenance_mode.unwrap_or(old.maintenance_mode),
            ytdlp_updates_paused: b.ytdlp_updates_paused.unwrap_or(old.ytdlp_updates_paused),
        };
        app.store.set_admin_flags(&new).await?;
        app.notices.set_flags(new);
        app.sources.ytdlp().set_updates_paused(new.ytdlp_updates_paused);
        info!(
            maintenance_mode = new.maintenance_mode,
            ytdlp_updates_paused = new.ytdlp_updates_paused,
            "admin changed the runtime flags"
        );
        anyhow::Ok(())
    })
    .await?
    .map_err(db_error)?;
    Ok(Json(tweaks_json(&st)))
}

/// `POST /api/actions/ytdlp-update`: start a yt-dlp update check on the bot's runtime (one at a time).
pub async fn ytdlp_update(_: Admin, State(st): State<AdminState>) -> AdminResult<(StatusCode, Json<Value>)> {
    {
        let mut job = st.ytdlp_job.lock().unwrap_or_else(|p| p.into_inner());
        if job.running {
            drop(job);
            return Ok((StatusCode::ACCEPTED, Json(tweaks_json(&st))));
        }
        job.running = true;
    }
    info!("admin started a yt-dlp update check");
    let app = st.app().clone();
    let job = st.ytdlp_job.clone();
    st.deps.main.spawn(async move {
        let ytdlp = app.sources.ytdlp().clone();
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
        app.ytdlp_version.set(version);
        *job.lock().unwrap_or_else(|p| p.into_inner()) = YtdlpJob {
            running: false,
            finished_at: Some(now_secs() * 1000),
            outcome: Some(outcome),
            version: label.and_then(|v| health::ytdlp_report(about::YtdlpLine::Known(v)).version),
        };
    });
    Ok((StatusCode::ACCEPTED, Json(tweaks_json(&st))))
}

/// `POST /api/actions/clear-stream-cache`
pub async fn clear_stream_cache(_: Admin, State(st): State<AdminState>) -> AdminResult<Json<Value>> {
    let app = st.app().clone();
    let removed = st
        .on_main(async move { app.sources.ytdlp().clear_stream_cache() })
        .await?;
    info!(removed, "admin cleared the stream URL cache");
    Ok(Json(json!({ "removed": removed })))
}

/// `POST /api/actions/clear-lookup-cache`
pub async fn clear_lookup_cache(_: Admin, State(st): State<AdminState>) -> AdminResult<Json<Value>> {
    let app = st.app().clone();
    let removed = st
        .on_main(async move { app.store.cache_clear().await })
        .await?
        .map_err(db_error)?;
    info!(removed, "admin cleared the lookup cache");
    Ok(Json(json!({ "removed": removed })))
}

fn auto_update_json(st: &AdminState) -> Value {
    let checker = &st.app().updates;
    let now = now_secs();
    json!({
        "settings": checker.settings().settings,
        "status": checker.status(),
        "runningCommit": updates::running_commit(),
        "nextWindow": checker.next_window(now).map(|t| t * 1000),
        "defaults": {
            "templates": updates::Templates::default(),
            "timezone": updates::default_timezone(),
        },
        "serverNow": now * 1000,
    })
}

/// `GET /api/auto-update`: settings and status of the automatic update notices.
pub async fn get_auto_update(_: Admin, State(st): State<AdminState>) -> Json<Value> {
    Json(auto_update_json(&st))
}

/// `PUT /api/auto-update`: save the settings (kept across restarts) and re-check right away.
pub async fn put_auto_update(
    _: Admin,
    State(st): State<AdminState>,
    Body(b): Body<Settings>,
) -> AdminResult<Json<Value>> {
    let valid = b.validate().map_err(AdminError::invalid)?;
    let raw = serde_json::to_string(&valid.settings).map_err(|_| AdminError::internal())?;
    let app = st.app().clone();
    let saved = valid.clone();
    st.on_main(async move {
        app.store.set_auto_update_settings(&raw).await?;
        app.updates.set_settings(saved);
        anyhow::Ok(())
    })
    .await?
    .map_err(db_error)?;
    let s = &valid.settings;
    info!(
        enabled = s.enabled,
        image = %s.image,
        frequency = ?s.frequency,
        time = %s.time,
        timezone = %s.timezone,
        "admin changed the automatic update notices"
    );
    Ok(Json(auto_update_json(&st)))
}

/// `POST /api/auto-update/check`: query the registry now (in the background).
pub async fn check_auto_update(_: Admin, State(st): State<AdminState>) -> (StatusCode, Json<Value>) {
    info!("admin started an update check");
    st.app().updates.check_now();
    (StatusCode::ACCEPTED, Json(auto_update_json(&st)))
}
