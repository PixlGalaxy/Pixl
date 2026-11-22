//! The signed-in user's own data: panel preferences (locale, history, appearance), taste summary,
//! the per-genre history drill-down (with removing a song), and the privacy wipe.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use super::{
    auth::Auth,
    error::{ApiError, ApiResult, Body, Params},
    media::rewrite_opt,
    ApiState,
};
use crate::{
    ai::taxonomy,
    store::{ForgetOutcome, GenreHistoryTrack, GenreWeight, UserPrefs, UserPrefsPatch},
};

const TOP_GENRES: u32 = 10;
const TOP_TRACKS: u32 = 10;
/// Page size of the genre drill-down (default and maximum).
const GENRE_PAGE: u32 = 50;

pub async fn get_prefs(State(st): State<ApiState>, Auth(s): Auth) -> ApiResult<Json<UserPrefs>> {
    let prefs = st
        .app
        .store
        .user_prefs(s.user_id)
        .await
        .map_err(|e| ApiError::internal("could not read prefs", e))?;
    Ok(Json(prefs))
}

/// Panel themes.
pub const THEMES: [&str; 3] = ["light", "dark", "system"];

/// Accent palettes the panel offers (web/src/lib/appearance.ts keeps the same list; `sky` is the
/// default). One per Tailwind color family, the same set as WebinCloud's skins.
pub const ACCENTS: [&str; 24] = [
    "sky",
    "indigo",
    "ocean",
    "sunset",
    "forest",
    "grape",
    "crimson",
    "solar",
    "mint",
    "blossom",
    "ares",
    "poseidon",
    "zeus",
    "jade",
    "verdigris",
    "nebula",
    "sakura",
    "slate",
    "mono",
    "graphite",
    "ash",
    "terracotta",
    "olive",
    "maroon",
];

fn valid_locale(l: &str) -> bool {
    (2..=10).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub async fn put_prefs(
    State(st): State<ApiState>,
    Auth(s): Auth,
    Body(patch): Body<UserPrefsPatch>,
) -> ApiResult<Json<UserPrefs>> {
    if let Some(Some(l)) = &patch.locale {
        if !valid_locale(l) {
            return Err(ApiError::invalid("invalid locale"));
        }
    }
    if let Some(Some(t)) = &patch.theme {
        if !THEMES.contains(&t.as_str()) {
            return Err(ApiError::invalid("theme must be light, dark or system"));
        }
    }
    if let Some(Some(a)) = &patch.accent {
        if !ACCENTS.contains(&a.as_str()) {
            return Err(ApiError::invalid("unknown accent"));
        }
    }
    let prefs = st
        .app
        .store
        .update_user_prefs(s.user_id, &patch)
        .await
        .map_err(|e| ApiError::internal("could not save prefs", e))?;
    // Feedback buttons depend on history being on.
    let _ = st.events.send(super::GuildEvent::User(s.user_id));
    Ok(Json(prefs))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TasteTrack {
    title: String,
    artists: Vec<String>,
    plays: u32,
    url: String,
    artwork_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Taste {
    top_genres: Vec<GenreWeight>,
    top_tracks: Vec<TasteTrack>,
}

pub async fn taste(State(st): State<ApiState>, Auth(s): Auth) -> ApiResult<Json<Taste>> {
    let store = &st.app.store;
    let (genres, tracks) = tokio::join!(
        store.top_genres(s.user_id, TOP_GENRES),
        store.top_tracks(s.user_id, TOP_TRACKS)
    );
    Ok(Json(Taste {
        top_genres: genres.map_err(|e| ApiError::internal("could not read taste", e))?,
        top_tracks: tracks
            .map_err(|e| ApiError::internal("could not read taste", e))?
            .into_iter()
            .map(|t| TasteTrack {
                title: t.title,
                artists: t.artists,
                plays: t.plays,
                url: t.url,
                artwork_url: rewrite_opt(t.artwork_url),
            })
            .collect(),
    }))
}

/// The drill-down and the removal only exist for the AI radio, and need the history on.
async fn require_history(st: &ApiState, user_id: u64) -> ApiResult<()> {
    if !st.app.config.ai_enabled {
        return Err(ApiError::not_found("AI features are turned off on this bot"));
    }
    let on = st
        .app
        .store
        .history_enabled(user_id)
        .await
        .map_err(|e| ApiError::internal("could not read prefs", e))?;
    if !on {
        return Err(ApiError::history_disabled());
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    offset: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenreTracks {
    tracks: Vec<GenreHistoryTrack>,
    total: u32,
}

/// `GET /api/me/taste/genre/{genre}`: the caller's history songs tagged with one taxonomy genre.
pub async fn genre_tracks(
    State(st): State<ApiState>,
    Auth(s): Auth,
    Path(genre): Path<String>,
    Params(q): Params<PageQuery>,
) -> ApiResult<Json<GenreTracks>> {
    require_history(&st, s.user_id).await?;
    let genre = taxonomy::canonical(genre.trim()).ok_or_else(|| ApiError::invalid("unknown genre"))?;
    let limit = q.limit.unwrap_or(GENRE_PAGE).clamp(1, GENRE_PAGE);
    let offset = q.offset.unwrap_or(0);
    let (mut tracks, total) = st
        .app
        .store
        .history_by_genre(s.user_id, genre, limit, offset)
        .await
        .map_err(|e| ApiError::internal("could not read history", e))?;
    for t in &mut tracks {
        t.artwork_url = rewrite_opt(t.artwork_url.take());
    }
    Ok(Json(GenreTracks { tracks, total }))
}

/// `DELETE /api/me/history/{trackKey}`: drop a song from the caller's history and dislike it, so
/// their radio never plays it; their taste is recomputed by the background job.
pub async fn forget_track(State(st): State<ApiState>, Auth(s): Auth, Path(key): Path<String>) -> ApiResult<StatusCode> {
    require_history(&st, s.user_id).await?;
    if key.is_empty() || key.len() > 2048 {
        return Err(ApiError::invalid("invalid track key"));
    }
    let outcome = st
        .app
        .store
        .forget_history_track(s.user_id, &key)
        .await
        .map_err(|e| ApiError::internal("could not update history", e))?;
    match outcome {
        ForgetOutcome::NotInHistory => Err(ApiError::not_found("That song is not in your history")),
        ForgetOutcome::Forgotten => {
            // Queue entries show the new dislike.
            let _ = st.events.send(super::GuildEvent::User(s.user_id));
            Ok(StatusCode::NO_CONTENT)
        }
    }
}

pub async fn delete_data(State(st): State<ApiState>, Auth(s): Auth) -> ApiResult<StatusCode> {
    let deleted = st
        .app
        .store
        .delete_user_data(s.user_id)
        .await
        .map_err(|e| ApiError::internal("could not delete user data", e))?;
    tracing::info!(
        user = s.user_id,
        history = deleted.history,
        feedback = deleted.feedback,
        playlists = deleted.playlists,
        "user data deleted from the panel"
    );
    let _ = st.events.send(super::GuildEvent::User(s.user_id));
    Ok(StatusCode::NO_CONTENT)
}
