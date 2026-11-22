//! `GET /api/search` (panel search box) and `GET /api/lyrics` (lyrics of the current track).

use std::time::Duration;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use super::{
    auth::Member,
    error::{ApiError, ApiResult, Params},
    media::rewrite_opt,
    player::ChannelQuery,
    ApiState,
};
use crate::sources::Lyrics;

const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
const LYRICS_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESULTS: usize = 10;

#[derive(Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    q: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    title: String,
    artists: Vec<String>,
    duration_ms: Option<u64>,
    artwork_url: Option<String>,
    url: String,
}

#[derive(Serialize)]
pub struct SearchResponse {
    results: Vec<SearchResult>,
}

pub async fn search(
    State(st): State<ApiState>,
    m: Member,
    Params(q): Params<SearchQuery>,
) -> ApiResult<Json<SearchResponse>> {
    let query = q.q.trim();
    if query.is_empty() {
        return Ok(Json(SearchResponse { results: Vec::new() }));
    }
    if query.len() > 200 {
        return Err(ApiError::invalid("search is too long"));
    }
    if !st.search_limit.allow(m.user_id()) {
        return Err(ApiError::rate_limited());
    }
    let found = match tokio::time::timeout(SEARCH_TIMEOUT, st.app.sources.search(query, MAX_RESULTS)).await {
        Ok(Ok(list)) => list,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "panel search failed");
            return Err(ApiError::unavailable("Search is not available right now"));
        }
        Err(_) => return Err(ApiError::unavailable("Search timed out")),
    };
    let results = found
        .into_iter()
        .take(MAX_RESULTS)
        .map(|t| SearchResult {
            title: t.title,
            artists: t.artists,
            duration_ms: t.duration_ms.filter(|d| *d > 0),
            artwork_url: rewrite_opt(t.thumbnail),
            url: t.url,
        })
        .collect();
    Ok(Json(SearchResponse { results }))
}

pub async fn lyrics(
    State(st): State<ApiState>,
    m: Member,
    Params(q): Params<ChannelQuery>,
) -> ApiResult<Json<Option<Lyrics>>> {
    let channel = st.own_channel(&m, &q.channel_id)?;
    let Some(p) = st.app.players.get(m.guild_id, channel) else {
        return Ok(Json(None));
    };
    let Some(track) = p.snapshot().current.as_ref().map(|e| e.info.clone()) else {
        return Ok(Json(None));
    };
    match tokio::time::timeout(LYRICS_TIMEOUT, st.app.sources.lyrics(&track)).await {
        Ok(Ok(found)) => Ok(Json(found)),
        Ok(Err(_)) => Ok(Json(None)),
        Err(_) => Err(ApiError::unavailable("Lyrics timed out")),
    }
}
