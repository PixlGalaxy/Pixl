//! Operator notices on the Activity side: `GET /api/notices` (public, read-only), the `notices`
//! WebSocket message and the maintenance-mode check of panel adds. Notices are managed only through
//! the separate admin listener (`super::admin`).

use axum::{
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{
    error::{ApiError, ApiResult},
    ApiState, GuildEvent,
};
use crate::store::now_secs;

/// `GET /api/notices`: the visible notices, soonest first, and whether maintenance mode is on.
pub async fn list(State(st): State<ApiState>) -> Response {
    let mut res = Json(st.app.notices.public_json(now_secs())).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// The `notices` WebSocket message.
pub fn ws_message(public: &Value) -> Value {
    json!({ "type": "notices", "notices": public["notices"], "maintenance": public["maintenance"] })
}

impl ApiState {
    /// Maintenance mode refuses new songs (adds, saved playlists, radio); playback goes on.
    pub fn maintenance_check(&self) -> ApiResult<()> {
        if !self.app.notices.maintenance_mode() {
            return Ok(());
        }
        let message = match self.app.notices.current(now_secs()) {
            Some((n, _)) if !n.title.trim().is_empty() => format!("Lynx is in maintenance: {}", n.title.trim()),
            _ => "Lynx is in maintenance. New songs can not be added right now.".to_owned(),
        };
        Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "maintenance", message))
    }
}

/// Push notice changes (edits in the admin panel, a notice appearing or ending) to every open panel.
pub async fn forward(st: ApiState, stop: CancellationToken) {
    let mut rx = st.app.notices.subscribe();
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            changed = rx.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
        let msg = ws_message(&st.app.notices.public_json(now_secs()));
        let _ = st.events.send(GuildEvent::Notices(msg));
    }
}
