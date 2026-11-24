//! Live logs: `GET /api/logs` (what the ring holds) and `GET /api/logs/stream` (Server-Sent
//! Events). A viewer that falls behind skips lines (`lagged` event) instead of slowing anything;
//! at most a few streams run at once, and each ends with its session or at shutdown.

use std::{convert::Infallible, time::Duration};

use axum::{
    extract::{Query, State},
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures_util::{stream, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use super::{auth::Admin, error::AdminError, AdminState};
use crate::logs;

/// How often a stream re-checks its session.
const SESSION_CHECK: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
pub struct After {
    #[serde(default)]
    after: u64,
}

/// `GET /api/logs?after=SEQ`
pub async fn recent(_: Admin, Query(q): Query<After>) -> Json<Value> {
    let hub = logs::global();
    Json(json!({ "lines": hub.since(q.after), "latest": hub.latest_seq() }))
}

fn line_event(line: &logs::LogLine) -> Event {
    Event::default()
        .event("line")
        .id(line.seq.to_string())
        .json_data(line)
        .unwrap_or_else(|_| Event::default().comment("unserializable"))
}

/// `GET /api/logs/stream?after=SEQ`: the ring after `after`, then new lines as they come.
pub async fn stream(
    Admin(token): Admin,
    State(st): State<AdminState>,
    Query(q): Query<After>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AdminError> {
    let slot = st.log_streams.clone().try_acquire_owned().map_err(|_| {
        AdminError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "too_many_streams",
            "Too many log viewers are open",
        )
    })?;
    let hub = logs::global();
    // Subscribe before reading the ring, so nothing falls in between (duplicates are skipped).
    let rx = hub.subscribe();
    let backlog = hub.since(q.after);
    let last = backlog.last().map(|l| l.seq).unwrap_or(q.after);
    let head = stream::iter(backlog.into_iter().map(|l| Ok(line_event(&l))));
    let stop = st.app().shutdown.clone();
    let check = tokio::time::interval(SESSION_CHECK);
    let live = stream::unfold(
        (rx, last, st, token, stop, check, slot),
        |(mut rx, mut last, st, token, stop, mut check, slot)| async move {
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return None,
                    _ = check.tick() => {
                        if !st.session_valid(&token) {
                            return None;
                        }
                    }
                    msg = rx.recv() => match msg {
                        Ok(line) if line.seq <= last => {}
                        Ok(line) => {
                            last = line.seq;
                            let ev = line_event(&line);
                            return Some((Ok(ev), (rx, last, st, token, stop, check, slot)));
                        }
                        Err(RecvError::Lagged(n)) => {
                            let ev = Event::default().event("lagged").data(n.to_string());
                            return Some((Ok(ev), (rx, last, st, token, stop, check, slot)));
                        }
                        Err(RecvError::Closed) => return None,
                    },
                }
            }
        },
    );
    Ok(Sse::new(head.chain(live)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}
