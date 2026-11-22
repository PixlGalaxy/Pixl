//! Apple Music links, resolved like Spotify (metadata here, lazy YouTube match before playing).
//! Songs and albums come from the free iTunes Lookup API (no key); playlists have no free API,
//! so the public playlist page's embedded JSON is read instead.

use std::{sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{sync::Mutex, time::Instant};

use super::{SourceError, SourceResult, TrackInfo, TrackOrigin, USER_AGENT};
use crate::store::Store;

const LOOKUP: &str = "https://itunes.apple.com/lookup";
const PAGE_BASE: &str = "https://music.apple.com";
pub const CACHE_NS: &str = "apple";
/// iTunes asks for about 20 requests per minute.
const MIN_INTERVAL: Duration = Duration::from_secs(3);
const ARTWORK: &str = "600x600bb";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub name: Option<String>,
    pub tracks: Vec<TrackInfo>,
}

#[derive(Clone)]
pub struct Apple {
    http: reqwest::Client,
    /// Next free request slot (shared by every caller).
    next: Arc<Mutex<Instant>>,
}

impl Apple {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            next: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Wait for the next polite request slot.
    async fn slot(&self) {
        let at = {
            let mut next = self.next.lock().await;
            let at = (*next).max(Instant::now());
            *next = at + MIN_INTERVAL;
            at
        };
        tokio::time::sleep_until(at).await;
    }

    async fn get(&self, url: &str) -> SourceResult<reqwest::Response> {
        self.slot().await;
        let res = self
            .http
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .timeout(Duration::from_secs(15))
            .send()
            .await?;
        match res.status() {
            s if s.is_success() => Ok(res),
            s if s.as_u16() == 403 || s.as_u16() == 429 => Err(SourceError::Http(
                "Apple Music is limiting requests, try again in a minute".into(),
            )),
            s if s.as_u16() == 404 => Err(SourceError::NotFound("Apple Music item not found".into())),
            s => Err(SourceError::Http(format!("Apple Music returned {s}"))),
        }
    }

    async fn cached<F, Fut>(&self, store: &Store, ttl: Duration, key: &str, fetch: F) -> SourceResult<Collection>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = SourceResult<Collection>>,
    {
        if let Ok(Some(hit)) = store.cache_get::<Collection>(CACHE_NS, key).await {
            return Ok(hit);
        }
        let c = fetch().await?;
        let _ = store.cache_put(CACHE_NS, key, &c, ttl).await;
        Ok(c)
    }

    async fn lookup(&self, country: &str, id: &str, songs: bool) -> SourceResult<Value> {
        let mut url = format!("{LOOKUP}?id={id}&country={country}");
        if songs {
            url.push_str("&entity=song&limit=200");
        }
        Ok(self.get(&url).await?.json().await?)
    }

    pub async fn song(&self, store: &Store, ttl: Duration, country: &str, id: &str) -> SourceResult<TrackInfo> {
        let c = self
            .cached(store, ttl, &format!("song:{country}:{id}"), || async {
                let (_, tracks) = parse_lookup(&self.lookup(country, id, false).await?);
                Ok(Collection { name: None, tracks })
            })
            .await?;
        c.tracks
            .into_iter()
            .next()
            .ok_or_else(|| SourceError::NotFound("Apple Music song not found".into()))
    }

    /// Album tracks in disc/track order.
    pub async fn album(
        &self,
        store: &Store,
        ttl: Duration,
        country: &str,
        id: &str,
        limit: usize,
    ) -> SourceResult<Collection> {
        let mut c = self
            .cached(store, ttl, &format!("album:{country}:{id}"), || async {
                let (name, tracks) = parse_lookup(&self.lookup(country, id, true).await?);
                Ok(Collection { name, tracks })
            })
            .await?;
        if c.tracks.is_empty() {
            return Err(SourceError::NotFound("Apple Music album not found".into()));
        }
        c.tracks.truncate(limit);
        Ok(c)
    }

    /// Public playlist (`pl.…`), read from the page.
    pub async fn playlist(
        &self,
        store: &Store,
        ttl: Duration,
        country: &str,
        id: &str,
        limit: usize,
    ) -> SourceResult<Collection> {
        let mut c = self
            .cached(store, ttl, &format!("playlist:{country}:{id}"), || async {
                let html = self
                    .get(&format!("{PAGE_BASE}/{country}/playlist/{id}"))
                    .await?
                    .text()
                    .await?;
                parse_playlist_page(&html)
                    .ok_or_else(|| SourceError::Unavailable("Apple Music playlist could not be read".into()))
            })
            .await?;
        if c.tracks.is_empty() {
            return Err(SourceError::NotFound("That Apple Music playlist is empty".into()));
        }
        c.tracks.truncate(limit);
        Ok(c)
    }
}

/// "Bad Bunny & Chencho Corleone", "A, B & C" -> names.
fn split_artists(s: &str) -> Vec<String> {
    s.split(", ")
        .flat_map(|p| p.split(" & "))
        .map(|a| a.trim().to_owned())
        .filter(|a| !a.is_empty())
        .collect()
}

/// `.../100x100bb.jpg` -> a 600x600 rendition.
fn upscale_artwork(url: &str) -> String {
    match url.rfind("/100x100bb") {
        Some(pos) => format!("{}/{ARTWORK}{}", &url[..pos], &url[pos + "/100x100bb".len()..]),
        None => url.to_owned(),
    }
}

/// Page artwork templates: `.../{w}x{h}bb.{f}`.
fn template_artwork(url: &str) -> String {
    url.replace("{w}x{h}bb.{f}", &format!("{ARTWORK}.jpg"))
        .replace("{w}x{h}bb", ARTWORK)
        .replace("{f}", "jpg")
}

/// Canonical page URL without tracking parameters (`uo=4`).
fn clean_url(url: &str) -> String {
    match url.split_once('?') {
        Some((base, q)) => {
            let kept: Vec<&str> = q.split('&').filter(|kv| kv.starts_with("i=")).collect();
            if kept.is_empty() {
                base.to_owned()
            } else {
                format!("{base}?{}", kept.join("&"))
            }
        }
        None => url.to_owned(),
    }
}

fn apple_track(
    title: String,
    artists: Vec<String>,
    duration_ms: Option<u64>,
    art: Option<String>,
    url: String,
) -> TrackInfo {
    TrackInfo {
        title,
        artists,
        duration_ms,
        thumbnail: art,
        url,
        origin: TrackOrigin::AppleMusic,
        youtube_id: None,
        spotify_id: None,
        isrc: None,
        music: None,
    }
}

/// iTunes Lookup response -> (collection name, songs in disc/track order).
pub(crate) fn parse_lookup(v: &Value) -> (Option<String>, Vec<TrackInfo>) {
    let results = v["results"].as_array().cloned().unwrap_or_default();
    let name = results
        .iter()
        .find(|r| r["wrapperType"] == "collection")
        .and_then(|r| r["collectionName"].as_str())
        .map(str::to_owned);
    let mut songs: Vec<&Value> = results
        .iter()
        .filter(|r| r["wrapperType"] == "track" && r["kind"] == "song")
        .collect();
    songs.sort_by_key(|r| {
        (
            r["discNumber"].as_u64().unwrap_or(1),
            r["trackNumber"].as_u64().unwrap_or(0),
        )
    });
    let tracks = songs
        .into_iter()
        .filter_map(|r| {
            let title = r["trackName"].as_str()?.to_owned();
            let url = r["trackViewUrl"].as_str().map(clean_url)?;
            Some(apple_track(
                title,
                split_artists(r["artistName"].as_str().unwrap_or("")),
                r["trackTimeMillis"].as_u64(),
                r["artworkUrl100"].as_str().map(upscale_artwork),
                url,
            ))
        })
        .collect();
    (name, tracks)
}

fn server_data(html: &str) -> Option<Value> {
    let start = html.find("id=\"serialized-server-data\"")?;
    let body = &html[start..];
    let open = body.find('>')? + 1;
    let close = body[open..].find("</script>")? + open;
    serde_json::from_str(body[open..close].trim()).ok()
}

/// Public playlist page -> name and tracks in order. `None` when the embedded data is missing or
/// has an unknown shape.
pub(crate) fn parse_playlist_page(html: &str) -> Option<Collection> {
    let data = server_data(html)?;
    let sections = data["data"]
        .as_array()?
        .iter()
        .find_map(|d| d["data"]["sections"].as_array())?;
    let kind = |s: &Value, k: &str| s["itemKind"].as_str() == Some(k);
    let name = sections
        .iter()
        .find(|s| kind(s, "containerDetailHeaderLockup"))
        .and_then(|s| s["items"][0]["title"].as_str())
        .map(str::to_owned);
    let list = sections.iter().find(|s| kind(s, "trackLockup"))?;
    let tracks: Vec<TrackInfo> = list["items"]
        .as_array()?
        .iter()
        .filter_map(|it| {
            let title = it["title"].as_str()?.to_owned();
            let artists: Vec<String> = it["subtitleLinks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| l["title"].as_str().map(str::to_owned))
                .collect();
            let url = it["contentDescriptor"]["url"]
                .as_str()
                .map(clean_url)
                .unwrap_or_default();
            let art = it["artwork"]["dictionary"]["url"].as_str().map(template_artwork);
            Some(apple_track(title, artists, it["duration"].as_u64(), art, url))
        })
        .collect();
    Some(Collection { name, tracks })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_lookup_album_in_track_order() {
        let song = |n: u64, disc: u64, title: &str, artist: &str| {
            json!({ "wrapperType": "track", "kind": "song", "trackId": 100 + n, "trackName": title, "artistName": artist,
                "trackNumber": n, "discNumber": disc, "trackTimeMillis": 200_000 + n,
                "trackViewUrl": format!("https://music.apple.com/us/album/x/1?i={}&uo=4", 100 + n),
                "artworkUrl100": "https://is1-ssl.mzstatic.com/image/thumb/Music/a.jpg/100x100bb.jpg" })
        };
        let v = json!({ "resultCount": 4, "results": [
            { "wrapperType": "collection", "collectionType": "Album", "collectionName": "Un Verano Sin Ti" },
            song(2, 1, "Me Porto Bonito", "Bad Bunny & Chencho Corleone"),
            song(1, 2, "Disc Two Opener", "Bad Bunny"),
            song(1, 1, "Moscow Mule", "Bad Bunny"),
            { "wrapperType": "track", "kind": "music-video", "trackName": "Video" }
        ] });
        let (name, t) = parse_lookup(&v);
        assert_eq!(name.as_deref(), Some("Un Verano Sin Ti"));
        let titles: Vec<&str> = t.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["Moscow Mule", "Me Porto Bonito", "Disc Two Opener"]);
        assert_eq!(t[1].artists, vec!["Bad Bunny", "Chencho Corleone"]);
        assert_eq!(t[0].duration_ms, Some(200_001));
        assert_eq!(t[0].url, "https://music.apple.com/us/album/x/1?i=101");
        assert_eq!(
            t[0].thumbnail.as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/Music/a.jpg/600x600bb.jpg")
        );
        assert_eq!(t[0].origin, TrackOrigin::AppleMusic);
        assert!(t[0].youtube_id.is_none());
        assert!(parse_lookup(&json!({ "resultCount": 0, "results": [] })).1.is_empty());
    }

    fn lockup(id: &str, title: &str, artists: &[&str], ms: u64) -> Value {
        json!({ "id": format!("track-lockup - pl.x - {id}"), "title": title, "duration": ms,
            "subtitleLinks": artists.iter().map(|a| json!({ "title": a })).collect::<Vec<_>>(),
            "tertiaryLinks": [ { "title": "Album" } ],
            "contentDescriptor": { "kind": "song", "identifiers": { "storeAdamID": id },
                "url": format!("https://music.apple.com/us/album/s/9?i={id}") },
            "artwork": { "dictionary": { "url": "https://is1-ssl.mzstatic.com/image/thumb/M/a.jpg/{w}x{h}bb.{f}" } } })
    }

    #[test]
    fn parses_playlist_page_fixture() {
        let data = json!({ "data": [ { "intent": { "$kind": "PlaylistDetailPageIntent" }, "data": { "sections": [
            { "id": "playlist-detail-header-section", "itemKind": "containerDetailHeaderLockup",
              "items": [ { "title": "Today\u{2019}s Hits", "trackCount": 2 } ] },
            { "id": "track-list", "itemKind": "trackLockup", "items": [
                lockup("1889992115", "stupid song", &["Olivia Rodrigo"], 209_680),
                lockup("1841769602", "Solar Eclipse", &["Drake", "Don Toliver"], 190_000) ] },
            { "id": "artist", "itemKind": "bubbleLockup", "items": [] }
        ] } } ], "userTokenHash": "x" });
        let html = format!(
            "<html><head><script type=\"application/json\" id=\"serialized-server-data\">{data}</script></head></html>"
        );
        let c = parse_playlist_page(&html).unwrap();
        assert_eq!(c.name.as_deref(), Some("Today\u{2019}s Hits"));
        assert_eq!(c.tracks.len(), 2);
        assert_eq!(c.tracks[1].title, "Solar Eclipse");
        assert_eq!(c.tracks[1].artists, vec!["Drake", "Don Toliver"]);
        assert_eq!(c.tracks[0].duration_ms, Some(209_680));
        assert_eq!(c.tracks[0].url, "https://music.apple.com/us/album/s/9?i=1889992115");
        assert_eq!(
            c.tracks[0].thumbnail.as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/M/a.jpg/600x600bb.jpg")
        );
        assert!(parse_playlist_page("<html>no data</html>").is_none());
        assert!(parse_playlist_page("<script id=\"serialized-server-data\">{\"data\": []}</script>").is_none());
    }

    #[test]
    fn small_helpers() {
        assert_eq!(split_artists("A, B & C"), vec!["A", "B", "C"]);
        assert_eq!(
            clean_url("https://music.apple.com/us/album/a/1?uo=4"),
            "https://music.apple.com/us/album/a/1"
        );
        assert_eq!(
            upscale_artwork("https://x/a.jpg/100x100bb.jpg"),
            "https://x/a.jpg/600x600bb.jpg"
        );
    }

    // ---- network (cargo test -- --ignored apple_live)

    #[tokio::test]
    #[ignore]
    async fn apple_live_song_album_playlist() {
        let apple = Apple::new(reqwest::Client::new());
        let path = std::env::temp_dir().join(format!("lynx-apple-{}.db", uuid::Uuid::new_v4()));
        let store = Store::open(&path).await.unwrap();
        let ttl = Duration::from_secs(600);

        let s = apple.song(&store, ttl, "us", "1622045625").await.unwrap();
        println!("song: {} / {:?} {:?}", s.title, s.artists, s.duration_ms);
        assert_eq!(s.title, "Moscow Mule");
        assert_eq!(s.artists, vec!["Bad Bunny"]);

        let a = apple.album(&store, ttl, "us", "1622045624", 100).await.unwrap();
        println!("album: {:?} {} tracks", a.name, a.tracks.len());
        assert_eq!(a.name.as_deref(), Some("Un Verano Sin Ti"));
        assert_eq!(a.tracks.len(), 23);
        assert_eq!(a.tracks[0].title, "Moscow Mule");

        let p = apple
            .playlist(&store, ttl, "us", "pl.f4d106fed2bd41149aaacabb233eb5eb", 20)
            .await
            .unwrap();
        println!(
            "playlist: {:?} {} tracks, first {:?}",
            p.name,
            p.tracks.len(),
            p.tracks.first().map(|t| (&t.title, &t.artists))
        );
        assert!(p.name.is_some());
        assert_eq!(p.tracks.len(), 20);
        assert!(p
            .tracks
            .iter()
            .all(|t| !t.artists.is_empty() && t.duration_ms.is_some()));

        store.close().await;
        let _ = std::fs::remove_file(path);
    }
}
