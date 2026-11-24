//! Saved playlists (Playlists tab): the caller's list, pin / remove, a list's songs (shared
//! 60-minute cache), a rate-limited refresh, and queueing a whole list or one of its songs through
//! the same checks as `/api/queue/add`.
//!
//! Refreshes go to Spotify / YouTube / Apple, so the server limits them per user (the panel only
//! mirrors it): one every [`REFRESH_INTERVAL`] (extra clicks are answered from the cache), and
//! [`SPAM_HITS`] requests inside [`SPAM_WINDOW`] block that user's refreshes for [`BLOCK`].

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use super::{
    auth::{Auth, Member},
    error::{ApiError, ApiResult, Body},
    media::rewrite_opt,
    queue::{play_error, play_request, Added},
    ApiState,
};
use crate::{
    discord::play,
    procs::ProcOwner,
    sources::{CachedList, ListRef, Resolved, TrackInfo},
    store::{NewPlaylist, SaveOutcome, SavedPlaylist},
};

/// A refresh is honored at most this often per user.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
/// Refresh requests (honored or not) counted for spam detection ...
pub const SPAM_WINDOW: Duration = Duration::from_secs(5);
/// ... this many of them inside the window block the user's refreshes ...
pub const SPAM_HITS: usize = 10;
/// ... for this long.
pub const BLOCK: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshDecision {
    /// Go upstream.
    Allow,
    /// Too soon after the last honored refresh: answer from the cache.
    Ignore,
    /// Blocked for spamming; the time left.
    Blocked(Duration),
}

#[derive(Default)]
struct RefreshState {
    last: Option<Instant>,
    hits: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

/// Per-user refresh limiter (see the module docs). Bounded like the other limiters.
pub struct RefreshLimiter {
    map: DashMap<u64, RefreshState>,
    max_keys: usize,
}

impl RefreshLimiter {
    pub fn new(max_keys: usize) -> Self {
        Self {
            map: DashMap::new(),
            max_keys: max_keys.max(1),
        }
    }

    pub fn check(&self, user: u64) -> RefreshDecision {
        self.check_at(user, Instant::now())
    }

    pub fn check_at(&self, user: u64, now: Instant) -> RefreshDecision {
        if self.map.len() >= self.max_keys && !self.map.contains_key(&user) {
            self.sweep_at(now);
        }
        let mut e = self.map.entry(user).or_default();
        if let Some(until) = e.blocked_until {
            if now < until {
                return RefreshDecision::Blocked(until - now);
            }
            // The block is over: start counting again from zero.
            e.blocked_until = None;
            e.hits.clear();
        }
        while e.hits.front().is_some_and(|t| now.duration_since(*t) >= SPAM_WINDOW) {
            e.hits.pop_front();
        }
        e.hits.push_back(now);
        if e.hits.len() >= SPAM_HITS {
            e.hits.clear();
            e.blocked_until = Some(now + BLOCK);
            return RefreshDecision::Blocked(BLOCK);
        }
        if e.last.is_some_and(|l| now.duration_since(l) < REFRESH_INTERVAL) {
            return RefreshDecision::Ignore;
        }
        e.last = Some(now);
        RefreshDecision::Allow
    }

    pub fn sweep(&self) {
        self.sweep_at(Instant::now());
    }

    fn sweep_at(&self, now: Instant) {
        let keep = REFRESH_INTERVAL.max(SPAM_WINDOW);
        self.map.retain(|_, s| {
            s.blocked_until.is_some_and(|u| now < u) || s.last.is_some_and(|l| now.duration_since(l) < keep) || {
                s.hits.back().is_some_and(|h| now.duration_since(*h) < keep)
            }
        });
        if self.map.len() >= self.max_keys {
            self.map.clear();
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiPlaylist {
    id: String,
    name: String,
    /// `spotify` | `youtube` | `youtube_music` | `apple_music`.
    platform: String,
    url: String,
    image_url: Option<String>,
    pinned: bool,
    added_at: i64,
    used_at: i64,
}

impl From<SavedPlaylist> for ApiPlaylist {
    fn from(p: SavedPlaylist) -> Self {
        Self {
            id: p.id.to_string(),
            name: p.name,
            platform: p.platform,
            url: p.url,
            image_url: rewrite_opt(p.image_url),
            pinned: p.pinned,
            added_at: p.added_at,
            used_at: p.used_at,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistsResponse {
    playlists: Vec<ApiPlaylist>,
    /// `MAX_STORED_PLAYLISTS`.
    max: usize,
    /// New lists are saved for this user (saving is on and their listening history is on).
    saving: bool,
    /// Every slot is pinned: new lists are not saved until one is unpinned or removed.
    full: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistTrack {
    title: String,
    artists: Vec<String>,
    duration_ms: Option<u64>,
    artwork_url: Option<String>,
    url: String,
}

impl From<&TrackInfo> for PlaylistTrack {
    fn from(t: &TrackInfo) -> Self {
        Self {
            title: t.display_title().to_owned(),
            artists: t.display_artists().to_vec(),
            duration_ms: t.duration_ms,
            artwork_url: rewrite_opt(t.thumbnail.clone()),
            url: t.url.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistTracks {
    playlist: ApiPlaylist,
    tracks: Vec<PlaylistTrack>,
    /// Unix seconds of the fetch the songs come from.
    fetched_at: i64,
    /// This response comes from a new fetch made for the refresh.
    refreshed: bool,
}

fn parse_id(raw: &str) -> ApiResult<i64> {
    raw.trim()
        .parse::<i64>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| ApiError::invalid("invalid id"))
}

fn gone() -> ApiError {
    ApiError::not_found("That playlist is not in your list")
}

impl ApiState {
    async fn saved_playlist(&self, user_id: u64, raw_id: &str) -> ApiResult<(SavedPlaylist, ListRef)> {
        let id = parse_id(raw_id)?;
        let p = self
            .app
            .store
            .playlist(user_id, id)
            .await
            .map_err(|e| ApiError::internal("could not read playlists", e))?
            .ok_or_else(gone)?;
        let list = ListRef::parse(&p.url).ok_or_else(|| ApiError::invalid("That playlist link is not supported"))?;
        Ok((p, list))
    }

    /// The list's songs (shared cache, or a fetch; `force` always fetches).
    async fn list_tracks(&self, list: &ListRef, force: bool, guild_id: u64) -> ApiResult<CachedList> {
        let owner = ProcOwner::new(guild_id, 0, None);
        play::saved_list_tracks(self.play_ctx(), list, force, &owner)
            .await
            .map_err(play_error)
    }

    /// Keep the saved name / cover in step with what the platform says now.
    async fn sync_meta(&self, user_id: u64, p: &mut SavedPlaylist, c: &CachedList) {
        let name = c.name.as_deref().unwrap_or_default();
        let name_changed = !name.is_empty() && name != p.name;
        let image_changed = c.image.is_some() && c.image != p.image_url;
        if !name_changed && !image_changed {
            return;
        }
        if self
            .app
            .store
            .update_playlist_meta(user_id, p.id, name, c.image.as_deref())
            .await
            .is_ok()
        {
            if name_changed {
                p.name = name.to_owned();
            }
            if image_changed {
                p.image_url = c.image.clone();
            }
        }
    }

    async fn tracks_response(
        &self,
        user_id: u64,
        mut p: SavedPlaylist,
        c: CachedList,
        refreshed: bool,
    ) -> Json<PlaylistTracks> {
        self.sync_meta(user_id, &mut p, &c).await;
        Json(PlaylistTracks {
            playlist: p.into(),
            tracks: c.tracks.iter().map(PlaylistTrack::from).collect(),
            fetched_at: c.fetched_at,
            refreshed,
        })
    }
}

/// `GET /api/playlists`: the caller's saved lists, pinned first, then most recently used.
pub async fn list(State(st): State<ApiState>, Auth(s): Auth) -> ApiResult<Json<PlaylistsResponse>> {
    let store = &st.app.store;
    let max = st.app.config.max_stored_playlists;
    let saved = store
        .playlists(s.user_id)
        .await
        .map_err(|e| ApiError::internal("could not read playlists", e))?;
    let history = store.history_enabled(s.user_id).await.unwrap_or(true);
    let saving = max > 0 && history;
    let pinned = saved.iter().filter(|p| p.pinned).count();
    Ok(Json(PlaylistsResponse {
        full: saving && pinned >= max,
        playlists: saved.into_iter().map(ApiPlaylist::from).collect(),
        max,
        saving,
    }))
}

#[derive(Deserialize)]
pub struct PinBody {
    pinned: bool,
}

/// `POST /api/playlists/{id}/pin`: pinned lists stay on top and are never dropped when full.
pub async fn pin(
    State(st): State<ApiState>,
    Auth(s): Auth,
    Path(id): Path<String>,
    Body(b): Body<PinBody>,
) -> ApiResult<StatusCode> {
    let found = st
        .app
        .store
        .set_playlist_pinned(s.user_id, parse_id(&id)?, b.pinned)
        .await
        .map_err(|e| ApiError::internal("could not update playlist", e))?;
    if found {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(gone())
    }
}

/// `DELETE /api/playlists/{id}`.
pub async fn remove(State(st): State<ApiState>, Auth(s): Auth, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let found = st
        .app
        .store
        .remove_playlist(s.user_id, parse_id(&id)?)
        .await
        .map_err(|e| ApiError::internal("could not remove playlist", e))?;
    if found {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(gone())
    }
}

/// `GET /api/playlists/{id}/tracks`: the songs, from the shared cache when it is fresh.
pub async fn tracks(
    State(st): State<ApiState>,
    Auth(s): Auth,
    Path(id): Path<String>,
) -> ApiResult<Json<PlaylistTracks>> {
    let (p, list) = st.saved_playlist(s.user_id, &id).await?;
    let c = st.list_tracks(&list, false, s.guild_id.unwrap_or(0)).await?;
    Ok(st.tracks_response(s.user_id, p, c, false).await)
}

/// `POST /api/playlists/{id}/refresh`: fetch the songs again (rate limited per user, see the
/// module docs). A refresh inside the interval answers from the cache with `refreshed: false`.
pub async fn refresh(
    State(st): State<ApiState>,
    Auth(s): Auth,
    Path(id): Path<String>,
) -> ApiResult<Json<PlaylistTracks>> {
    let (p, list) = st.saved_playlist(s.user_id, &id).await?;
    let guild = s.guild_id.unwrap_or(0);
    match st.refresh_limit.check(s.user_id) {
        RefreshDecision::Blocked(left) => {
            let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
            Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "refresh_blocked",
                "Too many refreshes. Please wait 5 minutes",
            )
            .with_retry_after(secs))
        }
        RefreshDecision::Ignore => {
            // Only the cache; an expired one is read again like opening the list would.
            let c = match st.app.sources.cached_list(&list).await {
                Some(c) => c,
                None => st.list_tracks(&list, false, guild).await?,
            };
            Ok(st.tracks_response(s.user_id, p, c, false).await)
        }
        RefreshDecision::Allow => {
            let c = st.list_tracks(&list, true, guild).await?;
            Ok(st.tracks_response(s.user_id, p, c, true).await)
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddBody {
    channel_id: String,
    /// One song of the list (its `url`); absent: the whole list.
    #[serde(default)]
    track_url: Option<String>,
}

/// `POST /api/playlists/{id}/add`: queue the list (or one song of it) like `/api/queue/add` would,
/// from the shared cache. Queueing the whole list moves it to the top of the saved lists.
pub async fn add(
    State(st): State<ApiState>,
    m: Member,
    Path(id): Path<String>,
    Body(b): Body<AddBody>,
) -> ApiResult<Json<Added>> {
    let channel = st.own_channel(&m, &b.channel_id)?;
    let user = m.user_id();
    let (p, list) = st.saved_playlist(user, &id).await?;
    st.add_checks(&m, channel, false).await?;
    let c = st.list_tracks(&list, false, m.guild_id).await?;
    let req = play_request(&m, channel, "");
    let played = match b.track_url.as_deref().map(str::trim) {
        // The whole list: the same path as `/playlist`.
        None => play::play_saved(st.play_ctx(), req, &p, &list, c).await,
        Some(url) => {
            let i = c
                .tracks
                .iter()
                .position(|t| t.url == url)
                .ok_or_else(|| ApiError::not_found("That song is no longer in the playlist"))?;
            let spec = play::ListSpec {
                origin: play::saved_origin(&p, &list, &c),
                first_index: u32::try_from(i + 1).unwrap_or(u32::MAX),
            };
            let resolved = Resolved {
                tracks: vec![c.tracks[i].clone()],
                playlist_name: c.name,
                image: c.image,
            };
            play::play_tracks(st.play_ctx(), req, resolved, Some(spec)).await
        }
    }
    .map_err(play_error)?;
    Ok(Json(Added {
        added: played.push.added,
    }))
}

#[derive(Deserialize)]
pub struct SaveBody {
    /// A list link (normally the `source.url` of a queue entry).
    url: String,
}

#[derive(Serialize)]
pub struct SavedResponse {
    playlist: ApiPlaylist,
}

/// `POST /api/playlists/save`: save a list (by its link) to the caller's playlists, the same way
/// queueing it does: not when saving is off or the caller's listening history is off, pinned
/// lists are never dropped for it and a full list of pinned ones refuses it.
pub async fn save(
    State(st): State<ApiState>,
    Auth(s): Auth,
    Body(b): Body<SaveBody>,
) -> ApiResult<Json<SavedResponse>> {
    let url = b.url.trim();
    if url.is_empty() || url.len() > 500 {
        return Err(ApiError::invalid("url must be 1-500 characters"));
    }
    let list = ListRef::parse(url).ok_or_else(|| ApiError::invalid("That list can't be saved"))?;
    let store = &st.app.store;
    let max = st.app.config.max_stored_playlists;
    if max == 0 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "saving_off",
            "Saving playlists is turned off on this bot",
        ));
    }
    if !store.history_enabled(s.user_id).await.unwrap_or(false) {
        return Err(ApiError::history_disabled());
    }
    // Name and cover from the shared cache (the list is normally queued, so it is there).
    let c = match st.app.sources.cached_list(&list).await {
        Some(c) => c,
        None => {
            if !st.add_limit.allow(s.user_id) {
                return Err(ApiError::rate_limited());
            }
            st.list_tracks(&list, false, s.guild_id.unwrap_or(0)).await?
        }
    };
    let p = NewPlaylist {
        url: &list.url,
        platform: list.platform.as_str(),
        name: c.name.as_deref().unwrap_or_default(),
        image_url: c.image.as_deref(),
    };
    let id = match store
        .save_playlist(s.user_id, &p, max)
        .await
        .map_err(|e| ApiError::internal("could not save playlist", e))?
    {
        SaveOutcome::Saved { id, .. } => id,
        SaveOutcome::Full => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "playlists_full",
                "Every saved playlist is pinned. Unpin or remove one first",
            ))
        }
    };
    let saved = store
        .playlist(s.user_id, id)
        .await
        .map_err(|e| ApiError::internal("could not read playlists", e))?
        .ok_or_else(gone)?;
    Ok(Json(SavedResponse { playlist: saved.into() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_interval_spam_block_and_reblock() {
        let l = RefreshLimiter::new(100);
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert_eq!(l.check_at(1, at(0)), RefreshDecision::Allow);
        assert_eq!(
            l.check_at(1, at(1_000)),
            RefreshDecision::Ignore,
            "inside 5 s: no upstream request"
        );
        assert_eq!(l.check_at(1, at(4_900)), RefreshDecision::Ignore);
        assert_eq!(
            l.check_at(1, at(5_000)),
            RefreshDecision::Allow,
            "5 s after the honored one"
        );
        assert_eq!(l.check_at(2, at(5_000)), RefreshDecision::Allow, "per user");

        // Slow clicking never blocks.
        let slow = RefreshLimiter::new(100);
        for i in 0..30 {
            assert_ne!(
                slow.check_at(1, at(i * 1_000)),
                RefreshDecision::Blocked(BLOCK),
                "one click a second is not spam"
            );
        }

        // 10 clicks in ~4.5 s: blocked for 5 minutes.
        let spam = RefreshLimiter::new(100);
        let mut last = RefreshDecision::Allow;
        for i in 0..SPAM_HITS as u64 {
            last = spam.check_at(1, at(i * 500));
        }
        assert_eq!(last, RefreshDecision::Blocked(BLOCK));
        let RefreshDecision::Blocked(left) = spam.check_at(1, at(4_500 + 60_000)) else {
            panic!("still blocked a minute later");
        };
        assert_eq!(left, BLOCK - Duration::from_secs(60));
        assert!(matches!(
            spam.check_at(1, at(4_500 + 299_000)),
            RefreshDecision::Blocked(_)
        ));
        assert_eq!(
            spam.check_at(2, at(10_000)),
            RefreshDecision::Allow,
            "other users are not blocked"
        );

        // The block ends; a normal refresh works again ...
        let after = 4_500 + BLOCK.as_millis() as u64;
        assert_eq!(spam.check_at(1, at(after)), RefreshDecision::Allow);
        // ... and spamming again blocks for another 5 minutes.
        let mut last = RefreshDecision::Allow;
        for i in 1..SPAM_HITS as u64 {
            last = spam.check_at(1, at(after + i * 300));
        }
        assert_eq!(last, RefreshDecision::Blocked(BLOCK));
    }

    #[test]
    fn refresh_limiter_stays_bounded() {
        let l = RefreshLimiter::new(3);
        let now = Instant::now();
        for u in 0..10 {
            l.check_at(u, now);
        }
        assert!(l.map.len() <= 3);
        // Old entries are swept.
        l.sweep_at(now + BLOCK);
        assert!(l.map.is_empty());
    }
}
