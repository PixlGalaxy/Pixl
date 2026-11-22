//! Spotify Web API with Client Credentials: tracks, albums and playlists (fully paginated, in order).

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use reqwest::StatusCode;
use serde_json::Value;
use tokio::sync::Mutex;

use super::{SourceError, SourceResult, TrackInfo, TrackOrigin};

const API: &str = "https://api.spotify.com/v1";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
/// Refresh this long before the token actually expires.
const REFRESH_MARGIN: Duration = Duration::from_secs(120);
const MAX_RETRIES: usize = 3;

#[derive(Clone)]
pub struct Spotify {
    http: reqwest::Client,
    client_id: Arc<str>,
    client_secret: Arc<str>,
    token: Arc<Mutex<Option<(String, Instant)>>>,
}

#[derive(Debug, Clone)]
pub struct Collection {
    pub name: Option<String>,
    /// Album cover / playlist image.
    pub image: Option<String>,
    pub tracks: Vec<TrackInfo>,
}

enum Kind {
    Album,
    Playlist,
}

impl Spotify {
    pub fn new(http: reqwest::Client, client_id: &str, client_secret: &str) -> Self {
        Self {
            http,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            token: Arc::new(Mutex::new(None)),
        }
    }

    /// One shared token; whoever finds it stale refreshes it while the others wait on the lock.
    async fn token(&self, force: bool) -> SourceResult<String> {
        let mut guard = self.token.lock().await;
        if !force {
            if let Some((tok, exp)) = guard.as_ref() {
                if Instant::now() + REFRESH_MARGIN < *exp {
                    return Ok(tok.clone());
                }
            }
        }
        let res = self
            .http
            .post(TOKEN_URL)
            .basic_auth(&*self.client_id, Some(&*self.client_secret))
            .form(&[("grant_type", "client_credentials")])
            .timeout(Duration::from_secs(15))
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(SourceError::Http(format!("Spotify token request failed ({})", res.status())));
        }
        let v: Value = res.json().await?;
        let tok = v["access_token"].as_str().ok_or_else(|| SourceError::Http("Spotify token missing".into()))?;
        let ttl = v["expires_in"].as_u64().unwrap_or(3600);
        *guard = Some((tok.to_owned(), Instant::now() + Duration::from_secs(ttl)));
        Ok(tok.to_owned())
    }

    /// GET with auth, one forced token refresh on 401 and Retry-After handling on 429.
    async fn get(&self, url: &str) -> SourceResult<(StatusCode, Value)> {
        let mut refreshed = false;
        let mut retries = 0;
        loop {
            let tok = self.token(false).await?;
            let res = self.http.get(url).bearer_auth(&tok).timeout(Duration::from_secs(20)).send().await?;
            let status = res.status();
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                *self.token.lock().await = None;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS && retries < MAX_RETRIES {
                retries += 1;
                let wait = res
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(2)
                    .min(30);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            if status.is_server_error() && retries < MAX_RETRIES {
                retries += 1;
                tokio::time::sleep(Duration::from_millis(500 * retries as u64)).await;
                continue;
            }
            let body = res.json().await.unwrap_or(Value::Null);
            return Ok((status, body));
        }
    }

    async fn get_ok(&self, url: &str, what: &str) -> SourceResult<Value> {
        let (status, v) = self.get(url).await?;
        if status.is_success() {
            return Ok(v);
        }
        Err(status_error(status, what))
    }

    pub async fn track(&self, id: &str) -> SourceResult<TrackInfo> {
        let v = self.get_ok(&format!("{API}/tracks/{id}"), "track").await?;
        parse_track(&v, None).ok_or_else(|| SourceError::NotFound("Spotify track not found".into()))
    }

    pub async fn album(&self, id: &str, limit: usize) -> SourceResult<Collection> {
        let v = self.get_ok(&format!("{API}/albums/{id}"), "album").await?;
        let name = v["name"].as_str().map(str::to_owned);
        let cover = best_image(&v["images"]);
        let mut tracks = Vec::new();
        collect_page(&v["tracks"], cover.as_deref(), &mut tracks, Kind::Album);
        let mut next = v["tracks"]["next"].as_str().map(str::to_owned);
        while tracks.len() < limit {
            let Some(url) = next.take() else { break };
            let page = self.get_ok(&url, "album").await?;
            collect_page(&page, cover.as_deref(), &mut tracks, Kind::Album);
            next = page["next"].as_str().map(str::to_owned);
        }
        tracks.truncate(limit);
        Ok(Collection { name, image: cover, tracks })
    }

    pub async fn playlist(&self, id: &str, limit: usize) -> SourceResult<Collection> {
        // Metadata first: a 404 here is the clear "editorial/algorithmic playlist" case.
        let meta = self.get_ok(&format!("{API}/playlists/{id}?fields=name,images"), "playlist").await?;
        let name = meta["name"].as_str().map(str::to_owned);
        let image = best_image(&meta["images"]);
        let mut tracks = Vec::new();
        let mut next = Some(format!("{API}/playlists/{id}/tracks?limit=100&offset=0"));
        let mut tried_items = false;
        while tracks.len() < limit {
            let Some(url) = next.take() else { break };
            let (status, page) = self.get(&url).await?;
            if !status.is_success() {
                // Newer API revisions moved entries to /items; try it once on the first page.
                if !tried_items && tracks.is_empty() && matches!(status.as_u16(), 403 | 404 | 410) {
                    tried_items = true;
                    next = Some(format!("{API}/playlists/{id}/items?limit=100&offset=0"));
                    continue;
                }
                return Err(status_error(status, "playlist"));
            }
            collect_page(&page, None, &mut tracks, Kind::Playlist);
            next = page["next"].as_str().map(str::to_owned);
        }
        tracks.truncate(limit);
        Ok(Collection { name, image, tracks })
    }
}

fn status_error(status: StatusCode, what: &str) -> SourceError {
    match status.as_u16() {
        404 if what == "playlist" => SourceError::NotFound(
            "This Spotify playlist is not available. Playlists made by Spotify (editorial or personalized mixes) \
             cannot be read; copy it to your own playlist first."
                .into(),
        ),
        404 => SourceError::NotFound(format!("Spotify {what} not found")),
        401 | 403 => SourceError::Http(format!("Spotify refused access to this {what} ({status})")),
        429 => SourceError::Http("Spotify rate limit reached, try again shortly".into()),
        _ => SourceError::Http(format!("Spotify {what} request failed ({status})")),
    }
}

fn best_image(v: &Value) -> Option<String> {
    v.as_array()?
        .iter()
        .max_by_key(|i| i["width"].as_u64().unwrap_or(0))
        .and_then(|i| i["url"].as_str())
        .map(str::to_owned)
}

fn collect_page(page: &Value, cover: Option<&str>, out: &mut Vec<TrackInfo>, kind: Kind) {
    for it in page["items"].as_array().into_iter().flatten() {
        let t = match kind {
            Kind::Album => parse_track(it, cover),
            // Playlist entries wrap the track (`track`, or `item` in newer revisions).
            Kind::Playlist => {
                let inner = if it["track"].is_object() { &it["track"] } else { &it["item"] };
                if it["is_local"].as_bool() == Some(true) || inner["is_local"].as_bool() == Some(true) {
                    None
                } else {
                    parse_track(inner, cover)
                }
            }
        };
        if let Some(t) = t {
            out.push(t);
        }
    }
}

pub(crate) fn parse_track(v: &Value, album_cover: Option<&str>) -> Option<TrackInfo> {
    // Podcast episodes in playlists are skipped.
    if v["type"].as_str().is_some_and(|t| t != "track") {
        return None;
    }
    let id = v["id"].as_str()?;
    let title = v["name"].as_str().filter(|n| !n.is_empty())?.to_owned();
    let artists = v["artists"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x["name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    Some(TrackInfo {
        title,
        artists,
        duration_ms: v["duration_ms"].as_u64(),
        thumbnail: best_image(&v["album"]["images"]).or_else(|| album_cover.map(str::to_owned)),
        url: format!("https://open.spotify.com/track/{id}"),
        origin: TrackOrigin::Spotify,
        youtube_id: None,
        spotify_id: Some(id.to_owned()),
        isrc: v["external_ids"]["isrc"].as_str().map(|s| s.to_ascii_uppercase()),
        music: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn track(id: &str, name: &str) -> Value {
        json!({ "type": "track", "id": id, "name": name, "duration_ms": 200000,
            "artists": [ { "name": "Bad Bunny" }, { "name": "Chencho Corleone" } ],
            "external_ids": { "isrc": "qm6mz2214990" },
            "album": { "images": [ { "url": "small", "width": 64 }, { "url": "big", "width": 640 } ] } })
    }

    #[test]
    fn maps_tracks() {
        let t = parse_track(&track("4uLU6hMCjMI75M1A2tKUQC", "Me Porto Bonito"), None).unwrap();
        assert_eq!(t.artists, vec!["Bad Bunny", "Chencho Corleone"]);
        assert_eq!(t.isrc.as_deref(), Some("QM6MZ2214990"));
        assert_eq!(t.thumbnail.as_deref(), Some("big"));
        assert_eq!(t.url, "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC");
        assert!(t.youtube_id.is_none());
        assert_eq!(t.origin, TrackOrigin::Spotify);
        assert!(parse_track(&json!({ "type": "episode", "id": "x", "name": "Pod" }), None).is_none());
    }

    #[test]
    fn playlist_pages_keep_order_and_skip_locals() {
        let page = json!({ "items": [
            { "track": track("a", "One") },
            { "is_local": true, "track": track("l", "Local") },
            { "track": null },
            { "item": track("b", "Two") },
            { "track": track("c", "Three") } ] });
        let mut out = Vec::new();
        collect_page(&page, None, &mut out, Kind::Playlist);
        let names: Vec<_> = out.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(names, ["One", "Two", "Three"]);
    }

    #[test]
    fn album_tracks_use_album_cover() {
        let mut t = track("a", "One");
        t.as_object_mut().unwrap().remove("album");
        let page = json!({ "items": [ t ] });
        let mut out = Vec::new();
        collect_page(&page, Some("cover"), &mut out, Kind::Album);
        assert_eq!(out[0].thumbnail.as_deref(), Some("cover"));
    }

    #[test]
    fn editorial_404_is_explained() {
        let e = status_error(StatusCode::NOT_FOUND, "playlist");
        assert!(e.to_string().contains("cannot be read"));
    }

    // ---- network: needs SPOTIFY_CLIENT_ID / SPOTIFY_CLIENT_SECRET in the environment.

    fn client() -> Option<Spotify> {
        let id = std::env::var("SPOTIFY_CLIENT_ID").ok().filter(|s| !s.is_empty())?;
        let secret = std::env::var("SPOTIFY_CLIENT_SECRET").ok().filter(|s| !s.is_empty())?;
        Some(Spotify::new(reqwest::Client::new(), &id, &secret))
    }

    #[tokio::test]
    #[ignore]
    async fn spotify_track_album_playlist() {
        let Some(s) = client() else {
            eprintln!("Spotify credentials not set; skipping");
            return;
        };
        let t = s.track("4uLU6hMCjMI75M1A2tKUQC").await.unwrap();
        assert!(t.isrc.is_some());
        let a = s.album("4aawyAB9vmqN3uQ7FjRGTy", 500).await.unwrap();
        assert!(!a.tracks.is_empty());
        let p = s.playlist("3cEYpjA9oz9GiPac4AsH4n", 500).await.unwrap();
        assert!(!p.tracks.is_empty());
        let e = s.playlist("37i9dQZF1DXcBWIGoYBM5M", 50).await;
        println!("editorial playlist: {e:?}");
    }
}
