//! The signed-in user's own data: panel preferences (locale, history, appearance), taste summary and the
//! privacy wipe.

use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;

use super::{
    auth::Auth,
    error::{ApiError, ApiResult, Body},
    ApiState,
};
use crate::store::{GenreWeight, UserPrefs, UserPrefsPatch};

const TOP_GENRES: u32 = 10;
const TOP_TRACKS: u32 = 10;

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
            })
            .collect(),
    }))
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
        "user data deleted from the panel"
    );
    let _ = st.events.send(super::GuildEvent::User(s.user_id));
    Ok(StatusCode::NO_CONTENT)
}
