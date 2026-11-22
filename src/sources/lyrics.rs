//! Lyrics from LRCLIB (https://lrclib.net): exact lookup, then search, synced LRC parsed to lines.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{matcher::clean_title, text::normalize, SourceResult, TrackInfo};
use crate::store::Store;

const BASE: &str = "https://lrclib.net/api";
pub const CACHE_NS: &str = "lyrics";
/// Misses are cached for a shorter time; LRCLIB keeps growing.
const MISS_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricLine {
    pub time_ms: u64,
    pub line: String,
}

/// Shape of `GET /api/lyrics` in the panel contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lyrics {
    pub synced: Option<Vec<LyricLine>>,
    pub plain: Option<String>,
    pub source: String,
}

#[derive(Clone)]
pub struct Lrclib {
    http: reqwest::Client,
}

impl Lrclib {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    async fn get_json(&self, url: &str, query: &[(&str, String)]) -> SourceResult<Option<Value>> {
        let res = self
            .http
            .get(url)
            .query(query)
            .header(reqwest::header::USER_AGENT, super::USER_AGENT)
            .timeout(Duration::from_secs(15))
            .send()
            .await?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let res = res.error_for_status()?;
        Ok(Some(res.json().await?))
    }

    /// Lyrics for a track, cached in SQLite (hits for `ttl`, misses for a day).
    pub async fn lyrics(&self, store: &Store, ttl: Duration, track: &TrackInfo) -> SourceResult<Option<Lyrics>> {
        let artist = track.artists.first().cloned().unwrap_or_default();
        let title = strip_artist_prefix(&track.title, &artist);
        let secs = track.duration_ms.map(|d| (d + 500) / 1000);
        let key = format!("{}|{}|{}", normalize(&artist), normalize(&title), secs.unwrap_or(0));
        if let Ok(Some(hit)) = store.cache_get::<Option<Lyrics>>(CACHE_NS, &key).await {
            return Ok(hit);
        }

        let mut found = None;
        if !artist.is_empty() {
            let mut q = vec![("artist_name", artist.clone()), ("track_name", title.clone())];
            if let Some(s) = secs {
                q.push(("duration", s.to_string()));
            }
            if let Some(v) = self.get_json(&format!("{BASE}/get"), &q).await? {
                found = to_lyrics(&v);
            }
        }
        if found.is_none() {
            let mut q = vec![("track_name", title.clone())];
            if !artist.is_empty() {
                q.push(("artist_name", artist.clone()));
            }
            if let Some(Value::Array(list)) = self.get_json(&format!("{BASE}/search"), &q).await? {
                found = pick_search(&list, &title, secs).and_then(to_lyrics);
            }
        }
        let ttl = if found.is_some() { ttl } else { MISS_TTL };
        let _ = store.cache_put(CACHE_NS, &key, &found, ttl).await;
        Ok(found)
    }
}

/// YouTube titles often read "Artist - Title (Official Video)"; LRCLIB wants just the title.
fn strip_artist_prefix(title: &str, artist: &str) -> String {
    let t = match title.split_once(" - ") {
        Some((head, rest)) if !artist.is_empty() && normalize(head).contains(&normalize(artist)) => rest,
        _ => title,
    };
    // Drop bracketed noise like "(Official Video)" but keep the raw casing otherwise.
    let mut out = String::new();
    let mut depth = 0;
    for c in t.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.is_empty() {
        t.trim().to_owned()
    } else {
        out
    }
}

fn pick_search<'a>(list: &'a [Value], title: &str, secs: Option<u64>) -> Option<&'a Value> {
    let want = clean_title(title);
    list.iter()
        .filter(|v| v["instrumental"].as_bool() != Some(true))
        .filter(|v| v["trackName"].as_str().is_some_and(|n| clean_title(n) == want))
        .filter(|v| match (secs, v["duration"].as_f64()) {
            (Some(s), Some(d)) => (d - s as f64).abs() <= 5.0,
            _ => true,
        })
        // Prefer entries that have synced lyrics.
        .max_by_key(|v| v["syncedLyrics"].as_str().is_some_and(|s| !s.is_empty()))
}

fn to_lyrics(v: &Value) -> Option<Lyrics> {
    let synced = v["syncedLyrics"].as_str().map(parse_lrc).filter(|l| !l.is_empty());
    let plain = v["plainLyrics"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            synced
                .as_ref()
                .map(|l| l.iter().map(|x| x.line.as_str()).collect::<Vec<_>>().join("\n"))
        });
    if synced.is_none() && plain.is_none() {
        return None;
    }
    Some(Lyrics {
        synced,
        plain,
        source: "lrclib".into(),
    })
}

/// Parse LRC text: `[mm:ss.xx]` stamps (several per line allowed), `[offset:+/-ms]`, metadata tags skipped.
pub fn parse_lrc(lrc: &str) -> Vec<LyricLine> {
    let mut offset: i64 = 0;
    let mut out = Vec::new();
    for raw in lrc.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(body) = rest.strip_prefix('[') {
            let Some(end) = body.find(']') else { break };
            let tag = &body[..end];
            rest = &body[end + 1..];
            if let Some(v) = tag.strip_prefix("offset:") {
                offset = v.trim().parse().unwrap_or(0);
            } else if let Some(ms) = parse_stamp(tag) {
                stamps.push(ms);
            }
        }
        let text = strip_word_stamps(rest);
        for ms in stamps {
            // LRC offset: positive means lyrics come earlier.
            let t = (ms as i64 - offset).max(0) as u64;
            out.push(LyricLine {
                time_ms: t,
                line: text.clone(),
            });
        }
    }
    out.sort_by_key(|l| l.time_ms);
    out
}

/// Enhanced LRC carries per-word stamps (`<00:19.25>word`); the panel only needs the line.
fn strip_word_stamps(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('<') {
        match rest[open..].find('>') {
            Some(close) if parse_stamp(&rest[open + 1..open + close]).is_some() => {
                out.push_str(&rest[..open]);
                rest = &rest[open + close + 1..];
            }
            _ => {
                out.push_str(&rest[..=open]);
                rest = &rest[open + 1..];
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_stamp(tag: &str) -> Option<u64> {
    let (m, s) = tag.split_once(':')?;
    let m: u64 = m.trim().parse().ok()?;
    let (sec, frac) = match s.split_once(['.', ':']) {
        Some((a, b)) => (a, b),
        None => (s, "0"),
    };
    let sec: u64 = sec.trim().parse().ok()?;
    if sec >= 60 || !frac.chars().all(|c| c.is_ascii_digit()) || frac.is_empty() {
        return None;
    }
    // ".5" = 500 ms, ".05" = 50 ms, ".005" = 5 ms.
    let frac_ms = match frac.len() {
        1 => frac.parse::<u64>().ok()? * 100,
        2 => frac.parse::<u64>().ok()? * 10,
        _ => frac[..3].parse::<u64>().ok()?,
    };
    Some(m * 60_000 + sec * 1000 + frac_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_lrc() {
        let lrc = "[ar:Rick Astley]\n[ti:Never]\n[00:18.68] We're no strangers to love\n[00:22.86]You know the rules\n\
                   [01:02.5][00:30.00]Chorus\n[00:40.123]\n";
        let l = parse_lrc(lrc);
        assert_eq!(l.len(), 5);
        assert_eq!(
            l[0],
            LyricLine {
                time_ms: 18_680,
                line: "We're no strangers to love".into()
            }
        );
        assert_eq!(
            l[2],
            LyricLine {
                time_ms: 30_000,
                line: "Chorus".into()
            }
        );
        assert_eq!(l[3].line, "");
        assert_eq!(l[3].time_ms, 40_123);
        assert_eq!(l[4].time_ms, 62_500);
    }

    #[test]
    fn strips_enhanced_word_stamps() {
        let l = parse_lrc("[00:18.89]<00:18.89>We're <00:19.25>no <00:19.53>strangers <1 < 2>");
        assert_eq!(l[0].line, "We're no strangers <1 < 2>");
    }

    #[test]
    fn honours_offset() {
        let l = parse_lrc("[offset:+500]\n[00:01.00]a\n[00:00.20]b");
        assert_eq!(l[0].time_ms, 0);
        assert_eq!(l[1].time_ms, 500);
    }

    #[test]
    fn maps_response_and_picks_search() {
        let v = json!({ "trackName": "Never Gonna Give You Up", "duration": 213.0, "instrumental": false,
            "plainLyrics": "We're no strangers", "syncedLyrics": "[00:18.68]We're no strangers" });
        let l = to_lyrics(&v).unwrap();
        assert_eq!(l.synced.as_ref().unwrap().len(), 1);
        assert_eq!(l.plain.as_deref(), Some("We're no strangers"));
        assert_eq!(serde_json::to_value(&l).unwrap()["synced"][0]["timeMs"], 18_680);
        assert!(to_lyrics(&json!({ "plainLyrics": null, "syncedLyrics": null })).is_none());

        let list = vec![
            json!({ "trackName": "Never Gonna Give You Up", "duration": 300.0, "syncedLyrics": "[00:01.00]x" }),
            json!({ "trackName": "Never Gonna Give You Up", "duration": 214.0, "plainLyrics": "plain only" }),
            json!({ "trackName": "Never Gonna Give You Up (Remastered)", "duration": 213.0, "syncedLyrics": "[00:01.00]y" }),
        ];
        let p = pick_search(&list, "Never Gonna Give You Up", Some(213)).unwrap();
        // Same cleaned title and within 5 s; synced preferred.
        assert_eq!(p["syncedLyrics"], "[00:01.00]y");
    }

    #[test]
    fn strips_artist_prefix_and_noise() {
        assert_eq!(
            strip_artist_prefix(
                "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)",
                "Rick Astley"
            ),
            "Never Gonna Give You Up"
        );
        assert_eq!(
            strip_artist_prefix("Here Comes The Sun - Remastered 2009", "The Beatles"),
            "Here Comes The Sun - Remastered 2009"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn lrclib_live() {
        let path = std::env::temp_dir().join(format!("lynx-lyrics-{}.db", uuid::Uuid::new_v4()));
        let store = Store::open(&path).await.unwrap();
        let t = TrackInfo {
            title: "Never Gonna Give You Up".into(),
            artists: vec!["Rick Astley".into()],
            duration_ms: Some(213_000),
            thumbnail: None,
            url: String::new(),
            origin: super::super::TrackOrigin::Youtube,
            youtube_id: None,
            spotify_id: None,
            isrc: None,
        };
        let l = Lrclib::new(reqwest::Client::new())
            .lyrics(&store, Duration::from_secs(60), &t)
            .await
            .unwrap()
            .unwrap();
        let synced = l.synced.unwrap();
        println!("{} synced lines, first: {:?}", synced.len(), synced.first());
        assert!(synced.len() > 10);
        store.close().await;
        let _ = std::fs::remove_file(path);
    }
}
