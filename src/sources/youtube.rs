//! YouTube and YouTube Music through InnerTube (no yt-dlp): video details, playlists with every
//! continuation page, WEB search and YouTube Music song search.

use serde_json::{json, Value};

use super::{text::parse_clock, SourceError, SourceResult, TrackInfo, TrackOrigin};

/// InnerTube client identities. Bump the versions here when YouTube starts rejecting them.
pub mod clients {
    pub const BROWSER_UA: &str =
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";

    pub const WEB_NAME: &str = "WEB";
    pub const WEB_VERSION: &str = "2.20261119.07.00";
    pub const WEB_BASE: &str = "https://www.youtube.com/youtubei/v1";
    pub const WEB_ORIGIN: &str = "https://www.youtube.com";

    pub const REMIX_NAME: &str = "WEB_REMIX";
    pub const REMIX_VERSION: &str = "1.20261117.06.00";
    pub const REMIX_BASE: &str = "https://music.youtube.com/youtubei/v1";
    pub const REMIX_ORIGIN: &str = "https://music.youtube.com";

    /// WEB search filter: type = video.
    pub const WEB_SEARCH_VIDEOS: &str = "EgIQAQ%3D%3D";
    /// YouTube Music search filter: songs only.
    pub const REMIX_SEARCH_SONGS: &str = "EgWKAQIIAWoMEA4QChADEAQQCRAF";
}

use clients::*;

#[derive(Clone, Copy)]
enum Client {
    Web,
    Remix,
}

impl Client {
    fn parts(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Client::Web => (WEB_NAME, WEB_VERSION, WEB_BASE, WEB_ORIGIN),
            Client::Remix => (REMIX_NAME, REMIX_VERSION, REMIX_BASE, REMIX_ORIGIN),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Playlist {
    pub title: Option<String>,
    pub tracks: Vec<TrackInfo>,
}

#[derive(Clone)]
pub struct YouTube {
    http: reqwest::Client,
}

impl YouTube {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    async fn call(&self, client: Client, endpoint: &str, mut body: Value) -> SourceResult<Value> {
        let (name, version, base, origin) = client.parts();
        body["context"] = json!({ "client": { "clientName": name, "clientVersion": version, "hl": "en", "gl": "US" } });
        let res = self
            .http
            .post(format!("{base}/{endpoint}?prettyPrint=false"))
            .header(reqwest::header::USER_AGENT, BROWSER_UA)
            .header(reqwest::header::ORIGIN, origin)
            .header(
                "X-Youtube-Client-Name",
                if matches!(client, Client::Web) { "1" } else { "67" },
            )
            .header("X-Youtube-Client-Version", version)
            .json(&body)
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            return Err(SourceError::Http(format!("YouTube {endpoint} returned {status}")));
        }
        Ok(res.json().await?)
    }

    /// Metadata of one video.
    pub async fn video(&self, id: &str, music: bool) -> SourceResult<TrackInfo> {
        let v = self.call(Client::Web, "player", json!({ "videoId": id })).await?;
        parse_video_details(&v, music).ok_or_else(|| {
            let reason = v["playabilityStatus"]["reason"].as_str().unwrap_or("Video unavailable");
            SourceError::Unavailable(reason.to_owned())
        })
    }

    /// Every item of a playlist, in order, following continuations until `limit` items.
    pub async fn playlist(&self, id: &str, music: bool, limit: usize) -> SourceResult<Playlist> {
        if id.starts_with("RD") {
            return self.mix(id, music, limit).await;
        }
        let first = self
            .call(Client::Web, "browse", json!({ "browseId": format!("VL{id}") }))
            .await?;
        let title = playlist_title(&first);
        let (mut tracks, mut token) = parse_playlist_page(first_items(&first), music);
        if tracks.is_empty() && token.is_none() {
            let alert = find_first(&first, "alertRenderer")
                .and_then(|a| runs_text(&a["text"]))
                .unwrap_or_else(|| "Playlist not found or private".into());
            return Err(SourceError::NotFound(alert));
        }
        // Guard against a token that loops back on itself.
        let mut pages = 0;
        while tracks.len() < limit && pages < 500 {
            let Some(tok) = token.take() else { break };
            let page = self.call(Client::Web, "browse", json!({ "continuation": tok })).await?;
            let items = page["onResponseReceivedActions"].as_array().and_then(|a| {
                a.iter()
                    .find_map(|x| x["appendContinuationItemsAction"]["continuationItems"].as_array())
            });
            let Some(items) = items else { break };
            let (more, next) = parse_playlist_page(Some(items), music);
            if more.is_empty() {
                break;
            }
            tracks.extend(more);
            token = next;
            pages += 1;
        }
        tracks.truncate(limit);
        Ok(Playlist { title, tracks })
    }

    /// Auto-generated mixes only exist in the watch-next panel.
    async fn mix(&self, id: &str, music: bool, limit: usize) -> SourceResult<Playlist> {
        let v = self.call(Client::Web, "next", json!({ "playlistId": id })).await?;
        let mut tracks = Vec::new();
        let mut items = Vec::new();
        find_all(&v, "playlistPanelVideoRenderer", &mut items);
        for r in items {
            let Some(vid) = r["videoId"].as_str() else { continue };
            let title = text_of(&r["title"]).unwrap_or_default();
            let channel = text_of(&r["shortBylineText"]).or_else(|| text_of(&r["longBylineText"]));
            let duration = text_of(&r["lengthText"]).and_then(|t| parse_clock(&t));
            tracks.push(web_track(
                vid,
                title,
                channel,
                duration,
                best_thumb(&r["thumbnail"]["thumbnails"]),
                music,
            ));
        }
        dedup(&mut tracks);
        tracks.truncate(limit);
        if tracks.is_empty() {
            return Err(SourceError::NotFound("Mix not found".into()));
        }
        let title = find_first(&v, "playlist").and_then(|p| p["title"].as_str().map(str::to_owned));
        Ok(Playlist { title, tracks })
    }

    /// Regular YouTube video search.
    pub async fn search_videos(&self, query: &str, limit: usize) -> SourceResult<Vec<TrackInfo>> {
        let v = self
            .call(
                Client::Web,
                "search",
                json!({ "query": query, "params": WEB_SEARCH_VIDEOS }),
            )
            .await?;
        let mut out = parse_web_search(&v);
        out.truncate(limit);
        Ok(out)
    }

    /// YouTube Music search restricted to songs. Also used with an ISRC as the query.
    pub async fn search_songs(&self, query: &str, limit: usize) -> SourceResult<Vec<TrackInfo>> {
        let v = self
            .call(
                Client::Remix,
                "search",
                json!({ "query": query, "params": REMIX_SEARCH_SONGS }),
            )
            .await?;
        let mut out = parse_music_search(&v);
        out.truncate(limit);
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing (pure, unit-tested with trimmed fixtures)

fn find_all<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k == key {
                    out.push(x);
                }
                find_all(x, key, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| find_all(x, key, out)),
        _ => {}
    }
}

fn find_first<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => m.get(key).or_else(|| m.values().find_map(|x| find_first(x, key))),
        Value::Array(a) => a.iter().find_map(|x| find_first(x, key)),
        _ => None,
    }
}

fn runs_text(v: &Value) -> Option<String> {
    let runs = v["runs"].as_array()?;
    let s: String = runs.iter().filter_map(|r| r["text"].as_str()).collect();
    (!s.is_empty()).then_some(s)
}

/// `{simpleText}`, `{runs}` or `{content}` text containers.
fn text_of(v: &Value) -> Option<String> {
    v["simpleText"]
        .as_str()
        .or_else(|| v["content"].as_str())
        .map(str::to_owned)
        .or_else(|| runs_text(v))
}

fn best_thumb(list: &Value) -> Option<String> {
    let arr = list.as_array()?;
    arr.iter()
        .max_by_key(|t| t["width"].as_u64().unwrap_or(0) * t["height"].as_u64().unwrap_or(0))
        .and_then(|t| t["url"].as_str())
        .map(|u| {
            if u.starts_with("//") {
                format!("https:{u}")
            } else {
                u.to_owned()
            }
        })
}

/// YouTube Music art URLs carry their size (`=w120-h120-...`); ask for a large one.
fn upscale_music_thumb(url: &str) -> String {
    if let Some(pos) = url.rfind("=w") {
        // "=w<digits>-h<digits>" followed by the rest of the options.
        let tail = &url[pos + 2..];
        let w = tail.find(|c: char| !c.is_ascii_digit()).unwrap_or(tail.len());
        if w > 0 {
            if let Some(after) = tail[w..].strip_prefix("-h") {
                let h = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
                if h > 0 {
                    return format!("{}=w544-h544{}", &url[..pos], &after[h..]);
                }
            }
        }
    }
    url.to_owned()
}

fn clean_channel(name: &str) -> String {
    let n = name.trim();
    let n = n.strip_suffix(" - Topic").unwrap_or(n);
    let n = n.strip_suffix("VEVO").filter(|s| !s.is_empty()).unwrap_or(n);
    n.trim().to_owned()
}

fn video_url(id: &str, music: bool) -> String {
    if music {
        format!("https://music.youtube.com/watch?v={id}")
    } else {
        format!("https://www.youtube.com/watch?v={id}")
    }
}

fn web_track(
    id: &str,
    title: String,
    channel: Option<String>,
    duration_ms: Option<u64>,
    thumb: Option<String>,
    music: bool,
) -> TrackInfo {
    TrackInfo {
        title,
        artists: channel
            .map(|c| clean_channel(&c))
            .filter(|c| !c.is_empty())
            .into_iter()
            .collect(),
        duration_ms,
        thumbnail: thumb.or_else(|| Some(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"))),
        url: video_url(id, music),
        origin: if music {
            TrackOrigin::YoutubeMusic
        } else {
            TrackOrigin::Youtube
        },
        youtube_id: Some(id.to_owned()),
        spotify_id: None,
        isrc: None,
    }
}

fn dedup(tracks: &mut Vec<TrackInfo>) {
    let mut seen = std::collections::HashSet::new();
    tracks.retain(|t| seen.insert(t.youtube_id.clone()));
}

pub(crate) fn parse_video_details(v: &Value, music: bool) -> Option<TrackInfo> {
    let d = &v["videoDetails"];
    let id = d["videoId"].as_str()?;
    let title = d["title"].as_str()?.to_owned();
    let duration = d["lengthSeconds"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(|s| s * 1000);
    let thumb = best_thumb(&v["microformat"]["playerMicroformatRenderer"]["thumbnail"]["thumbnails"])
        .or_else(|| best_thumb(&d["thumbnail"]["thumbnails"]));
    Some(web_track(
        id,
        title,
        d["author"].as_str().map(str::to_owned),
        duration,
        thumb,
        music,
    ))
}

fn playlist_title(v: &Value) -> Option<String> {
    v["metadata"]["playlistMetadataRenderer"]["title"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| find_first(v, "pageHeaderRenderer").and_then(|h| h["pageTitle"].as_str().map(str::to_owned)))
        .or_else(|| find_first(v, "playlistHeaderRenderer").and_then(|h| text_of(&h["title"])))
}

/// First array that holds playlist entries (lockups or the older `playlistVideoRenderer`).
fn first_items(v: &Value) -> Option<&Vec<Value>> {
    fn is_entry(x: &Value) -> bool {
        x.get("lockupViewModel").is_some() || x.get("playlistVideoRenderer").is_some()
    }
    match v {
        Value::Object(m) => m.values().find_map(first_items),
        Value::Array(a) if a.iter().any(is_entry) => Some(a),
        Value::Array(a) => a.iter().find_map(first_items),
        _ => None,
    }
}

fn parse_playlist_page(items: Option<&Vec<Value>>, music: bool) -> (Vec<TrackInfo>, Option<String>) {
    let mut tracks = Vec::new();
    let mut token = None;
    for it in items.into_iter().flatten() {
        if let Some(l) = it.get("lockupViewModel") {
            if let Some(t) = parse_lockup(l, music) {
                tracks.push(t);
            }
        } else if let Some(r) = it.get("playlistVideoRenderer") {
            if let Some(t) = parse_playlist_video_renderer(r, music) {
                tracks.push(t);
            }
        } else if it.get("continuationItemViewModel").is_some() || it.get("continuationItemRenderer").is_some() {
            // The token sits in a nested continuationCommand (innertubeCommand wrapper).
            let mut cmds = Vec::new();
            find_all(it, "continuationCommand", &mut cmds);
            token = cmds.iter().find_map(|c| c["token"].as_str()).map(str::to_owned);
        }
    }
    (tracks, token)
}

fn parse_lockup(l: &Value, music: bool) -> Option<TrackInfo> {
    if l["contentType"].as_str() != Some("LOCKUP_CONTENT_TYPE_VIDEO") {
        return None;
    }
    let id = l["contentId"].as_str()?;
    let meta = &l["metadata"]["lockupMetadataViewModel"];
    let title = meta["title"]["content"].as_str()?.to_owned();
    let channel = meta["metadata"]["contentMetadataViewModel"]["metadataRows"][0]["metadataParts"][0]["text"]
        ["content"]
        .as_str()
        .map(str::to_owned);
    let mut badges = Vec::new();
    find_all(&l["contentImage"], "thumbnailBadgeViewModel", &mut badges);
    let duration = badges.iter().filter_map(|b| b["text"].as_str()).find_map(parse_clock);
    let thumb = best_thumb(&l["contentImage"]["thumbnailViewModel"]["image"]["sources"]);
    Some(web_track(id, title, channel, duration, thumb, music))
}

fn parse_playlist_video_renderer(r: &Value, music: bool) -> Option<TrackInfo> {
    // Deleted/private entries have no length and are not playable.
    if r["isPlayable"].as_bool() == Some(false) {
        return None;
    }
    let id = r["videoId"].as_str()?;
    let title = text_of(&r["title"])?;
    let duration = r["lengthSeconds"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|s| s * 1000);
    Some(web_track(
        id,
        title,
        text_of(&r["shortBylineText"]),
        duration,
        best_thumb(&r["thumbnail"]["thumbnails"]),
        music,
    ))
}

pub(crate) fn parse_web_search(v: &Value) -> Vec<TrackInfo> {
    // Only the primary results; shelves ("People also watched") live under the same section list,
    // so take the top-level itemSectionRenderer entries.
    let mut out = Vec::new();
    let sections = v["contents"]["twoColumnSearchResultsRenderer"]["primaryContents"]["sectionListRenderer"]
        ["contents"]
        .as_array();
    for sec in sections.into_iter().flatten() {
        for it in sec["itemSectionRenderer"]["contents"].as_array().into_iter().flatten() {
            let r = &it["videoRenderer"];
            let Some(id) = r["videoId"].as_str() else { continue };
            // Live streams have no length; skip upcoming/live entries.
            let Some(duration) = text_of(&r["lengthText"]).and_then(|t| parse_clock(&t)) else {
                continue;
            };
            let Some(title) = text_of(&r["title"]) else { continue };
            let channel = text_of(&r["ownerText"]).or_else(|| text_of(&r["longBylineText"]));
            out.push(web_track(
                id,
                title,
                channel,
                Some(duration),
                best_thumb(&r["thumbnail"]["thumbnails"]),
                false,
            ));
        }
    }
    dedup(&mut out);
    out
}

pub(crate) fn parse_music_search(v: &Value) -> Vec<TrackInfo> {
    let mut items = Vec::new();
    find_all(v, "musicResponsiveListItemRenderer", &mut items);
    let mut out: Vec<TrackInfo> = items.into_iter().filter_map(parse_music_item).collect();
    dedup(&mut out);
    out
}

fn parse_music_item(it: &Value) -> Option<TrackInfo> {
    let col = |i: usize| &it["flexColumns"][i]["musicResponsiveListItemFlexColumnRenderer"]["text"];
    let title_runs = col(0)["runs"].as_array()?;
    let title: String = title_runs.iter().filter_map(|r| r["text"].as_str()).collect();
    let id = it["playlistItemData"]["videoId"]
        .as_str()
        .or_else(|| {
            title_runs
                .iter()
                .find_map(|r| r["navigationEndpoint"]["watchEndpoint"]["videoId"].as_str())
        })
        .or_else(|| {
            it["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]["musicPlayButtonRenderer"]
                ["playNavigationEndpoint"]["watchEndpoint"]["videoId"]
                .as_str()
        })?;

    // Second column: "Artist & Artist • Album • 3:45", split on the bullet separators.
    let runs = col(1)["runs"].as_array().cloned().unwrap_or_default();
    let mut groups: Vec<Vec<&Value>> = vec![Vec::new()];
    for r in &runs {
        if r["text"].as_str().is_some_and(|t| t.trim() == "\u{2022}") {
            groups.push(Vec::new());
        } else {
            groups.last_mut().unwrap().push(r);
        }
    }
    let page_type = |r: &Value| {
        r["navigationEndpoint"]["browseEndpoint"]["browseEndpointContextSupportedConfigs"]
            ["browseEndpointContextMusicConfig"]["pageType"]
            .as_str()
            .map(str::to_owned)
    };
    let mut artists: Vec<String> = runs
        .iter()
        .filter(|r| page_type(r).as_deref() == Some("MUSIC_PAGE_TYPE_ARTIST"))
        .filter_map(|r| r["text"].as_str().map(str::to_owned))
        .collect();
    if artists.is_empty() {
        // Artists without a channel page are plain text in the first group: "A, B & C".
        if let Some(g) = groups.first() {
            let s: String = g.iter().filter_map(|r| r["text"].as_str()).collect();
            artists = s
                .split([',', '&'])
                .map(|a| a.trim().to_owned())
                .filter(|a| !a.is_empty() && a != "Song" && a != "Video")
                .collect();
        }
    }
    let duration = groups
        .iter()
        .rev()
        .find_map(|g| {
            let s: String = g.iter().filter_map(|r| r["text"].as_str()).collect();
            parse_clock(&s)
        })
        .or_else(|| {
            it["fixedColumns"][0]["musicResponsiveListItemFixedColumnRenderer"]["text"]["runs"][0]["text"]
                .as_str()
                .and_then(parse_clock)
        });
    let thumb = best_thumb(&it["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"])
        .map(|u| upscale_music_thumb(&u));

    Some(TrackInfo {
        title,
        artists,
        duration_ms: duration,
        thumbnail: thumb.or_else(|| Some(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"))),
        url: video_url(id, true),
        origin: TrackOrigin::YoutubeMusic,
        youtube_id: Some(id.to_owned()),
        spotify_id: None,
        isrc: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artist_run(name: &str) -> Value {
        json!({ "text": name, "navigationEndpoint": { "browseEndpoint": { "browseId": "UCx",
            "browseEndpointContextSupportedConfigs": { "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_ARTIST" } } } } })
    }

    fn music_item(id: &str, title: &str, second: Vec<Value>) -> Value {
        json!({ "musicResponsiveListItemRenderer": {
            "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                { "url": "https://lh3.googleusercontent.com/abc=w60-h60-l90-rj", "width": 60, "height": 60 },
                { "url": "https://lh3.googleusercontent.com/abc=w120-h120-l90-rj", "width": 120, "height": 120 } ] } } },
            "flexColumns": [
                { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                    { "text": title, "navigationEndpoint": { "watchEndpoint": { "videoId": id } } } ] } } },
                { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": second } } }
            ],
            "playlistItemData": { "videoId": id }
        } })
    }

    #[test]
    fn parses_music_songs() {
        let v = json!({ "contents": { "tabbedSearchResultsRenderer": { "tabs": [ { "tabRenderer": { "content": {
            "sectionListRenderer": { "contents": [ { "musicShelfRenderer": { "contents": [
                music_item("juRFjpB5Ppg", "Tití Me Preguntó", vec![
                    artist_run("Bad Bunny"), json!({"text": " \u{2022} "}), json!({"text": "Un Verano Sin Ti"}),
                    json!({"text": " \u{2022} "}), json!({"text": "4:04"}) ]),
                music_item("xm8b1okbkbM", "La Jumpa", vec![
                    artist_run("Arcángel"), json!({"text": " & "}), artist_run("Bad Bunny"),
                    json!({"text": " \u{2022} "}), json!({"text": "SR. SANTOS"}), json!({"text": " \u{2022} "}), json!({"text": "4:16"}) ]),
                music_item("aaaaaaaaaaa", "Plain", vec![
                    json!({"text": "Some One, Other & Third"}), json!({"text": " \u{2022} "}), json!({"text": "2:01"}) ]),
                music_item("juRFjpB5Ppg", "Duplicate", vec![])
            ] } } ] } } } } ] } } });
        let r = parse_music_search(&v);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].title, "Tití Me Preguntó");
        assert_eq!(r[0].artists, vec!["Bad Bunny"]);
        assert_eq!(r[0].duration_ms, Some(244_000));
        assert_eq!(
            r[0].thumbnail.as_deref(),
            Some("https://lh3.googleusercontent.com/abc=w544-h544-l90-rj")
        );
        assert_eq!(r[0].url, "https://music.youtube.com/watch?v=juRFjpB5Ppg");
        assert_eq!(r[0].origin, TrackOrigin::YoutubeMusic);
        assert_eq!(r[1].artists, vec!["Arcángel", "Bad Bunny"]);
        assert_eq!(r[2].artists, vec!["Some One", "Other", "Third"]);
        assert_eq!(r[2].duration_ms, Some(121_000));
    }

    #[test]
    fn parses_web_search_and_skips_live() {
        let vr = |id: &str, len: Option<&str>| {
            let mut r = json!({ "videoId": id, "title": { "runs": [ { "text": format!("Title {id}") } ] },
                "ownerText": { "runs": [ { "text": "Rick Astley" } ] },
                "thumbnail": { "thumbnails": [ { "url": "https://i.ytimg.com/a.jpg", "width": 360, "height": 202 },
                                               { "url": "https://i.ytimg.com/b.jpg", "width": 720, "height": 404 } ] } });
            if let Some(l) = len {
                r["lengthText"] = json!({ "simpleText": l });
            }
            json!({ "videoRenderer": r })
        };
        let v = json!({ "contents": { "twoColumnSearchResultsRenderer": { "primaryContents": { "sectionListRenderer": {
            "contents": [ { "itemSectionRenderer": { "contents": [
                vr("dQw4w9WgXcQ", Some("3:34")), vr("liveliveliv", None), { "shelfRenderer": {} }, vr("other000000", Some("1:02:03"))
            ] } } ] } } } } });
        let r = parse_web_search(&v);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].youtube_id.as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(r[0].artists, vec!["Rick Astley"]);
        assert_eq!(r[0].duration_ms, Some(214_000));
        assert_eq!(r[0].thumbnail.as_deref(), Some("https://i.ytimg.com/b.jpg"));
        assert_eq!(r[1].duration_ms, Some(3_723_000));
    }

    fn lockup(id: &str, title: &str, channel: &str, len: &str) -> Value {
        json!({ "lockupViewModel": {
            "contentId": id, "contentType": "LOCKUP_CONTENT_TYPE_VIDEO",
            "contentImage": { "thumbnailViewModel": {
                "image": { "sources": [ { "url": "https://i.ytimg.com/vi/x/hqdefault.jpg", "width": 336, "height": 188 } ] },
                "overlays": [ { "thumbnailBottomOverlayViewModel": { "badges": [ { "thumbnailBadgeViewModel": { "text": len } } ] } } ] } },
            "metadata": { "lockupMetadataViewModel": { "title": { "content": title },
                "metadata": { "contentMetadataViewModel": { "metadataRows": [
                    { "metadataParts": [ { "text": { "content": channel } } ] },
                    { "metadataParts": [ { "text": { "content": "42M views" } } ] } ] } } } }
        } })
    }

    #[test]
    fn parses_playlist_first_page_and_continuation() {
        let cont = json!({ "continuationItemViewModel": { "continuationCommand": { "innertubeCommand": {
            "continuationCommand": { "token": "NEXT", "request": "CONTINUATION_REQUEST_TYPE_BROWSE" } } } } });
        let v = json!({
            "metadata": { "playlistMetadataRenderer": { "title": "Popular Music Videos" } },
            "contents": { "twoColumnBrowseResultsRenderer": { "tabs": [ { "tabRenderer": { "content": { "sectionListRenderer": {
                "contents": [ { "itemSectionRenderer": { "contents": [
                    lockup("aaaaaaaaaaa", "One", "Artist A - Topic", "3:55"),
                    lockup("bbbbbbbbbbb", "Two", "ArtistBVEVO", "4:01"),
                    { "lockupViewModel": { "contentType": "LOCKUP_CONTENT_TYPE_PLAYLIST", "contentId": "PLx" } },
                    cont
                ] } } ] } } } } ] } }
        });
        assert_eq!(playlist_title(&v).as_deref(), Some("Popular Music Videos"));
        let (tracks, token) = parse_playlist_page(first_items(&v), false);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].artists, vec!["Artist A"]);
        assert_eq!(tracks[1].artists, vec!["ArtistB"]);
        assert_eq!(tracks[0].duration_ms, Some(235_000));
        assert_eq!(token.as_deref(), Some("NEXT"));

        let legacy = vec![
            json!({ "playlistVideoRenderer": { "videoId": "ccccccccccc", "title": { "runs": [ { "text": "Three" } ] },
            "shortBylineText": { "runs": [ { "text": "Chan" } ] }, "lengthSeconds": "61", "thumbnail": { "thumbnails": [] } } }),
            json!({ "playlistVideoRenderer": { "videoId": "ddddddddddd", "isPlayable": false, "title": { "simpleText": "[Deleted video]" } } }),
        ];
        let (tracks, token) = parse_playlist_page(Some(&legacy), true);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].duration_ms, Some(61_000));
        assert_eq!(tracks[0].origin, TrackOrigin::YoutubeMusic);
        assert!(token.is_none());
    }

    #[test]
    fn parses_video_details() {
        let v = json!({ "playabilityStatus": { "status": "UNPLAYABLE" },
            "videoDetails": { "videoId": "dQw4w9WgXcQ", "title": "Never Gonna Give You Up", "lengthSeconds": "213",
                "author": "Rick Astley", "thumbnail": { "thumbnails": [ { "url": "https://i.ytimg.com/s.jpg", "width": 120, "height": 90 } ] } },
            "microformat": { "playerMicroformatRenderer": { "thumbnail": { "thumbnails": [
                { "url": "https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg", "width": 1280, "height": 720 } ] } } } });
        let t = parse_video_details(&v, false).unwrap();
        assert_eq!(t.duration_ms, Some(213_000));
        assert_eq!(t.artists, vec!["Rick Astley"]);
        assert_eq!(
            t.thumbnail.as_deref(),
            Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg")
        );
        assert!(parse_video_details(&json!({}), false).is_none());
    }

    #[test]
    fn thumb_upscale() {
        assert_eq!(
            upscale_music_thumb("https://x/abc=w120-h120-l90-rj"),
            "https://x/abc=w544-h544-l90-rj"
        );
        assert_eq!(
            upscale_music_thumb("https://i.ytimg.com/vi/x/hq.jpg"),
            "https://i.ytimg.com/vi/x/hq.jpg"
        );
    }

    // ---- network (run with: cargo test -- --ignored innertube_)

    fn yt() -> YouTube {
        YouTube::new(reqwest::Client::new())
    }

    #[tokio::test]
    #[ignore]
    async fn innertube_web_search() {
        let r = yt()
            .search_videos("rick astley never gonna give you up", 5)
            .await
            .unwrap();
        assert!(!r.is_empty());
        assert!(
            r.iter().any(|t| t.youtube_id.as_deref() == Some("dQw4w9WgXcQ")),
            "{r:#?}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn innertube_music_songs_and_isrc() {
        let r = yt().search_songs("bad bunny titi me pregunto", 5).await.unwrap();
        assert!(!r.is_empty());
        assert!(r[0].artists.iter().any(|a| a == "Bad Bunny"), "{r:#?}");
        assert!(r[0].duration_ms.is_some());
        let r = yt().search_songs("GBARL9300135", 3).await.unwrap();
        assert!(r[0].title.contains("Never Gonna Give You Up"), "{r:#?}");
    }

    #[tokio::test]
    #[ignore]
    async fn innertube_playlist_all_pages() {
        // Large public playlist (> 100 items) to exercise continuations.
        let p = yt()
            .playlist("PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI", false, 1000)
            .await
            .unwrap();
        assert!(p.title.is_some());
        assert!(p.tracks.len() > 100, "only {} tracks", p.tracks.len());
        let p2 = yt()
            .playlist("PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI", false, 150)
            .await
            .unwrap();
        assert_eq!(p2.tracks.len(), 150);
        // Same order as the full fetch.
        assert_eq!(p2.tracks[120].youtube_id, p.tracks[120].youtube_id);
    }

    #[tokio::test]
    #[ignore]
    async fn innertube_video() {
        let t = yt().video("dQw4w9WgXcQ", false).await.unwrap();
        assert!(t.title.contains("Never Gonna Give You Up"));
        assert!(t.duration_ms.unwrap() > 200_000);
    }
}
