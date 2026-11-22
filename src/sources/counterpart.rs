//! YouTube Music counterpart of a pasted YouTube video: the audio track ("Artist - Topic",
//! `MUSIC_VIDEO_TYPE_ATV`) of the same recording, whose metadata is clean (title, artist list,
//! album). Playback keeps the pasted video; only the metadata comes from the song.
//!
//! Also holds the reverse direction used by the playback fallback: a confident regular YouTube
//! video for a song.

use std::time::Duration;

use tracing::debug;

use super::{
    matcher::{self, clean_title, Candidate},
    text::{contains_phrase, normalize},
    youtube::{MusicVideoType, MusicWatch, PanelVideo, YouTube},
    MusicMeta, SourceResult, TrackInfo, TrackOrigin,
};
use crate::store::Store;

pub const CACHE_NS: &str = "ytm_counterpart";
/// Song vs video duration tolerance for a normal match.
const EXACT_MS: u64 = 5_000;
/// Official music videos often carry an intro/outro: allowed only with a strong title match.
const OMV_MS: u64 = 30_000;
/// Fallback video search: music videos with intros are fine, as long as the rest is confident.
const VIDEO_MS: u64 = 60_000;
/// Title similarity needed with the exact duration rule / the OMV rule.
const TITLE_SIM: f64 = 0.6;
const TITLE_SIM_STRONG: f64 = 0.85;

/// The YouTube Music song for `track` (cached, negative results too). `Err` only on network or
/// API errors, which are not cached.
pub async fn lookup(yt: &YouTube, store: &Store, ttl: Duration, track: &TrackInfo) -> SourceResult<Option<MusicMeta>> {
    let Some(id) = track.youtube_id.as_deref() else {
        return Ok(None);
    };
    if let Ok(Some(hit)) = store.cache_get::<Option<MusicMeta>>(CACHE_NS, id).await {
        return Ok(hit);
    }
    let watch = yt.music_watch(id).await?;
    let found = match decide(track, watch.as_ref()) {
        Decision::Done(m) => m,
        Decision::Search { sources, omv } => {
            let songs = yt.search_songs(&song_query(&sources[0]), 10).await?;
            confident_song(&sources, omv, &songs).map(song_meta)
        }
    };
    debug!(%id, found = ?found.as_ref().map(|m| &m.youtube_id), "youtube music counterpart");
    let _ = store.cache_put(CACHE_NS, id, &found, ttl).await;
    Ok(found)
}

#[derive(Debug)]
pub(crate) enum Decision {
    /// Known without searching.
    Done(Option<MusicMeta>),
    /// Search YouTube Music songs and match against these readings of the video (best first).
    Search { sources: Vec<TrackInfo>, omv: bool },
}

pub(crate) fn decide(track: &TrackInfo, watch: Option<&MusicWatch>) -> Decision {
    if let Some(w) = watch {
        match w.video.video_type {
            // Already the song: its own panel metadata is the clean one.
            Some(MusicVideoType::Atv) => return Decision::Done(Some(w.video.to_meta())),
            Some(MusicVideoType::Podcast) => return Decision::Done(None),
            _ => {}
        }
        // YouTube's own audio/video link.
        if let Some(c) = w
            .counterpart
            .as_ref()
            .filter(|c| c.video_type == Some(MusicVideoType::Atv))
        {
            return Decision::Done(Some(c.to_meta()));
        }
    }
    let kind = watch.and_then(|w| w.video.video_type);
    let official = matches!(kind, Some(MusicVideoType::Omv | MusicVideoType::OfficialSource));
    let panel = watch.map(|w| &w.video).filter(|_| official);
    let sources = readings(track, panel);
    if sources.is_empty() {
        return Decision::Done(None);
    }
    Decision::Search {
        sources,
        omv: kind == Some(MusicVideoType::Omv),
    }
}

/// Plausible (artist, title) readings of a track, best first, as TrackInfo for the matcher.
fn readings(track: &TrackInfo, panel: Option<&PanelVideo>) -> Vec<TrackInfo> {
    let base = |title: &str, artists: Vec<String>, duration: Option<u64>| TrackInfo {
        title: title.trim().to_owned(),
        artists,
        duration_ms: track.duration_ms.or(duration),
        music: None,
        ..track.clone()
    };
    let mut out = Vec::new();
    if let Some(m) = &track.music {
        out.push(base(&m.title, m.artists.clone(), None));
    }
    if matches!(track.origin, TrackOrigin::Spotify | TrackOrigin::YoutubeMusic) && track.music.is_none() {
        out.push(base(&track.title, track.artists.clone(), None));
    }
    if let Some(p) = panel.filter(|p| !p.artists.is_empty() && !p.title.is_empty()) {
        out.push(base(&p.title, p.artists.clone(), p.duration_ms));
    }
    // "Artist - Title" uploads, then the reversed "Title - Artist".
    if let Some((a, t)) = split_artist_title(&track.title) {
        out.push(base(t, split_artists(a), None));
        out.push(base(a, split_artists(t), None));
    }
    if !track.artists.is_empty() {
        out.push(base(&track.title, track.artists.clone(), None));
    }
    out.retain(|t| !t.artists.is_empty() && !clean_title(&t.title).is_empty());
    out
}

fn split_artist_title(title: &str) -> Option<(&str, &str)> {
    for sep in [" - ", " – ", " — ", " | "] {
        if let Some((a, t)) = title.split_once(sep) {
            if !a.trim().is_empty() && !t.trim().is_empty() {
                return Some((a.trim(), t.trim()));
            }
        }
    }
    None
}

/// "Bad Bunny ft. Chencho Corleone", "A, B & C", "A x B" -> names.
fn split_artists(s: &str) -> Vec<String> {
    let mut t = format!(" {s} ");
    for m in [
        " feat. ",
        " Feat. ",
        " FEAT. ",
        " ft. ",
        " Ft. ",
        " FT. ",
        " feat ",
        " ft ",
        " FT ",
        " featuring ",
        " x ",
        " X ",
        " & ",
        " with ",
    ] {
        t = t.replace(m, ",");
    }
    t.split(',')
        .map(|a| a.trim().to_owned())
        .filter(|a| !a.is_empty())
        .collect()
}

fn compact(s: &str) -> String {
    normalize(s).replace(' ', "")
}

/// Some source artist is one of the candidate's (whole words), or the other way round.
/// Compact comparison covers channel names written together ("LuisFonsi").
fn artists_match(src: &[String], cand: &[String]) -> bool {
    let ct = normalize(&cand.join(" , "));
    let st = normalize(&src.join(" , "));
    src.iter().any(|a| contains_phrase(&ct, &normalize(a)))
        || cand.iter().any(|a| contains_phrase(&st, &normalize(a)))
        || src
            .iter()
            .any(|a| cand.iter().any(|c| !compact(a).is_empty() && compact(a) == compact(c)))
}

fn song_query(src: &TrackInfo) -> String {
    let artists: Vec<&str> = src.artists.iter().take(2).map(String::as_str).collect();
    format!("{} {}", artists.join(" "), clean_title(&src.title))
}

/// The confident-match rule for a YouTube Music song candidate.
pub(crate) fn song_ok(src: &TrackInfo, cand: &TrackInfo, omv: bool) -> bool {
    let (Some(a), Some(b)) = (src.duration_ms, cand.duration_ms) else {
        return false;
    };
    let d = a.abs_diff(b);
    if !matcher::same_variants(&src.title, cand.display_title()) {
        return false;
    }
    if !artists_match(&src.artists, cand.display_artists()) {
        return false;
    }
    let sim = matcher::title_similarity(src, cand);
    (d <= EXACT_MS && sim >= TITLE_SIM) || (omv && d <= OMV_MS && sim >= TITLE_SIM_STRONG)
}

/// Best song that passes `song_ok` for any reading and the matcher's `ACCEPT` bar.
pub(crate) fn confident_song<'a>(sources: &[TrackInfo], omv: bool, songs: &'a [TrackInfo]) -> Option<&'a TrackInfo> {
    let mut best: Option<(&TrackInfo, i32)> = None;
    for src in sources {
        for c in songs.iter().filter(|c| c.youtube_id.is_some()) {
            if !song_ok(src, c, omv) {
                continue;
            }
            let sc = matcher::score(src, &Candidate::new(c.clone()));
            if sc >= matcher::ACCEPT && best.is_none_or(|(_, b)| sc > b) {
                best = Some((c, sc));
            }
        }
    }
    best.map(|(c, _)| c)
}

fn song_meta(c: &TrackInfo) -> MusicMeta {
    c.music.clone().unwrap_or_else(|| MusicMeta {
        youtube_id: c.youtube_id.clone().unwrap_or_default(),
        title: c.title.clone(),
        artists: c.artists.clone(),
        album: None,
        isrc: c.isrc.clone(),
    })
}

// ---- song -> regular video (playback fallback)

/// Clean readings of a track for the video search (song metadata first).
pub(crate) fn clean_readings(track: &TrackInfo) -> Vec<TrackInfo> {
    readings(track, None)
}

pub(crate) fn video_query(src: &TrackInfo) -> String {
    match src.artists.first() {
        Some(a) => format!("{a} - {}", src.title),
        None => src.title.clone(),
    }
}

/// Regular videos that confidently are the same recording, best first (ids).
pub(crate) fn confident_videos(sources: &[TrackInfo], videos: &[TrackInfo]) -> Vec<String> {
    let mut scored: Vec<(String, i32)> = Vec::new();
    for src in sources {
        for v in videos {
            let Some(id) = v.youtube_id.clone() else { continue };
            let Some(sc) = video_score(src, v) else { continue };
            match scored.iter_mut().find(|(i, _)| *i == id) {
                Some(e) => e.1 = e.1.max(sc),
                None => scored.push((id, sc)),
            }
        }
    }
    scored.sort_by_key(|(_, s)| std::cmp::Reverse(*s));
    scored.into_iter().map(|(id, _)| id).collect()
}

fn video_score(src: &TrackInfo, v: &TrackInfo) -> Option<i32> {
    let (Some(a), Some(b)) = (src.duration_ms, v.duration_ms) else {
        return None;
    };
    if a.abs_diff(b) > VIDEO_MS || !matcher::same_variants(&src.title, &v.title) {
        return None;
    }
    // The artist may only be in the video title ("Artist - Song") or the channel.
    let mut cand_names = v.artists.clone();
    cand_names.push(v.title.clone());
    if !artists_match(&src.artists, &cand_names) {
        return None;
    }
    if matcher::title_similarity(src, v) < TITLE_SIM {
        return None;
    }
    let cand = Candidate {
        track: v.clone(),
        channel: v.artists.first().cloned(),
        from_isrc: false,
    };
    let sc = matcher::score(src, &cand);
    (sc >= matcher::ACCEPT).then_some(sc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, channel: &str, secs: u64) -> TrackInfo {
        TrackInfo {
            title: title.into(),
            artists: vec![channel.into()],
            duration_ms: Some(secs * 1000),
            thumbnail: None,
            url: String::new(),
            origin: TrackOrigin::Youtube,
            youtube_id: Some("video000000".into()),
            spotify_id: None,
            isrc: None,
            music: None,
        }
    }

    fn song(id: &str, title: &str, artists: &[&str], secs: u64) -> TrackInfo {
        let artists: Vec<String> = artists.iter().map(|s| s.to_string()).collect();
        TrackInfo {
            title: title.into(),
            artists: artists.clone(),
            duration_ms: Some(secs * 1000),
            thumbnail: None,
            url: String::new(),
            origin: TrackOrigin::YoutubeMusic,
            youtube_id: Some(id.into()),
            spotify_id: None,
            isrc: None,
            music: Some(MusicMeta {
                youtube_id: id.into(),
                title: title.into(),
                artists,
                album: Some("Album".into()),
                isrc: None,
            }),
        }
    }

    fn panel(id: &str, title: &str, artists: &[&str], secs: u64, kind: MusicVideoType) -> PanelVideo {
        PanelVideo {
            id: id.into(),
            title: title.into(),
            artists: artists.iter().map(|s| s.to_string()).collect(),
            album: None,
            duration_ms: Some(secs * 1000),
            video_type: Some(kind),
        }
    }

    fn pick(t: &TrackInfo, watch: Option<&MusicWatch>, songs: &[TrackInfo]) -> Option<String> {
        match decide(t, watch) {
            Decision::Done(m) => m.map(|m| m.youtube_id),
            Decision::Search { sources, omv } => {
                confident_song(&sources, omv, songs).and_then(|s| s.youtube_id.clone())
            }
        }
    }

    #[test]
    fn atv_is_its_own_song_and_linked_counterpart_wins() {
        let t = track("Despacito", "Luis Fonsi - Topic", 229);
        let w = MusicWatch {
            video: panel(
                "video000000",
                "Despacito",
                &["Luis Fonsi", "Daddy Yankee"],
                229,
                MusicVideoType::Atv,
            ),
            counterpart: None,
        };
        let Decision::Done(Some(m)) = decide(&t, Some(&w)) else {
            panic!()
        };
        assert_eq!(m.youtube_id, "video000000");
        assert_eq!(m.artists, vec!["Luis Fonsi", "Daddy Yankee"]);

        let w = MusicWatch {
            video: panel("video000000", "Despacito", &["Luis Fonsi"], 282, MusicVideoType::Omv),
            counterpart: Some(panel(
                "atv00000000",
                "Despacito",
                &["Luis Fonsi", "Daddy Yankee"],
                229,
                MusicVideoType::Atv,
            )),
        };
        assert_eq!(pick(&t, Some(&w), &[]).as_deref(), Some("atv00000000"));

        let w = MusicWatch {
            video: panel("video000000", "Ep. 1", &["Pod"], 3000, MusicVideoType::Podcast),
            counterpart: None,
        };
        assert!(matches!(decide(&t, Some(&w)), Decision::Done(None)));
    }

    #[test]
    fn omv_maps_to_song_with_clean_metadata() {
        let t = track("Bad Bunny - Tití Me Preguntó (Video Oficial)", "Bad Bunny", 250);
        let w = MusicWatch {
            video: panel(
                "video000000",
                "Tití Me Preguntó",
                &["Bad Bunny"],
                250,
                MusicVideoType::Omv,
            ),
            counterpart: None,
        };
        let songs = [
            song("cover000000", "Titi Me Pregunto (Cover)", &["Someone"], 244),
            song("juRFjpB5Ppg", "Tití Me Preguntó", &["Bad Bunny"], 244),
        ];
        // 6 s off: only the OMV tolerance with a strong title match lets it through.
        assert_eq!(pick(&t, Some(&w), &songs).as_deref(), Some("juRFjpB5Ppg"));
        // The same 6 s as a user upload is not enough.
        let w_ugc = MusicWatch {
            video: panel("video000000", "x", &[], 250, MusicVideoType::Ugc),
            counterpart: None,
        };
        assert_eq!(pick(&t, Some(&w_ugc), &songs), None);
        // Beyond 30 s even an OMV is rejected (long intros are too risky).
        let mut long = t.clone();
        long.duration_ms = Some(290_000);
        let w = MusicWatch {
            video: panel(
                "video000000",
                "Tití Me Preguntó",
                &["Bad Bunny"],
                290,
                MusicVideoType::Omv,
            ),
            counterpart: None,
        };
        assert_eq!(pick(&long, Some(&w), &songs), None);
    }

    #[test]
    fn ugc_artist_title_upload_matches_exact_duration() {
        let t = track("Luis Fonsi - Despacito ft. Daddy Yankee", "LuisFonsiVEVO", 230);
        let songs = [song("atv00000000", "Despacito", &["Luis Fonsi", "Daddy Yankee"], 229)];
        assert_eq!(pick(&t, None, &songs).as_deref(), Some("atv00000000"));
        // Reversed "Title - Artist".
        let t = track("Despacito - Luis Fonsi", "Some Channel", 229);
        assert_eq!(pick(&t, None, &songs).as_deref(), Some("atv00000000"));
    }

    #[test]
    fn rejects_covers_live_and_other_artists() {
        let songs = [song("atv00000000", "Despacito", &["Luis Fonsi", "Daddy Yankee"], 229)];
        // A cover upload with the same title.
        let t = track("Despacito (Cover)", "Jane Doe Music", 229);
        assert_eq!(pick(&t, None, &songs), None);
        let t = track("Jane Doe - Despacito", "Jane Doe", 229);
        assert_eq!(pick(&t, None, &songs), None);
        // Live performance by the right artist.
        let t = track("Luis Fonsi - Despacito (Live)", "Luis Fonsi", 229);
        assert_eq!(pick(&t, None, &songs), None);
        // Live recording on YouTube Music vs a studio video.
        let live = [song("live0000000", "Despacito (Live)", &["Luis Fonsi"], 229)];
        let t = track("Luis Fonsi - Despacito", "Luis Fonsi", 229);
        assert_eq!(pick(&t, None, &live), None);
        // Same artist, different song.
        let t = track("Luis Fonsi - Échame La Culpa", "Luis Fonsi", 229);
        assert_eq!(pick(&t, None, &songs), None);
    }

    #[test]
    fn non_music_video_gets_nothing() {
        let t = track("I tested every budget laptop of 2026", "Tech Channel", 1210);
        let songs = [song("x0000000000", "Laptop", &["Some Band"], 200)];
        assert_eq!(pick(&t, None, &songs), None);
        let w = MusicWatch {
            video: panel("video000000", "x", &["Tech Channel"], 1210, MusicVideoType::Ugc),
            counterpart: None,
        };
        assert_eq!(pick(&t, Some(&w), &songs), None);
    }

    #[test]
    fn artist_splitting() {
        assert_eq!(
            split_artists("Bad Bunny ft. Chencho Corleone"),
            vec!["Bad Bunny", "Chencho Corleone"]
        );
        assert_eq!(split_artists("A, B & C"), vec!["A", "B", "C"]);
        assert_eq!(
            split_artists("Peso Pluma x Tito Double P"),
            vec!["Peso Pluma", "Tito Double P"]
        );
        assert!(artists_match(&["LuisFonsi".into()], &["Luis Fonsi".into()]));
        assert!(!artists_match(&["Jane Doe".into()], &["Luis Fonsi".into()]));
    }

    fn video(id: &str, title: &str, channel: &str, secs: u64) -> TrackInfo {
        let mut t = track(title, channel, secs);
        t.youtube_id = Some(id.into());
        t
    }

    #[test]
    fn fallback_videos_are_confident_only() {
        let s = song("atv00000000", "Despacito", &["Luis Fonsi", "Daddy Yankee"], 229);
        let srcs = clean_readings(&s);
        assert_eq!(video_query(&srcs[0]), "Luis Fonsi - Despacito");
        let vids = [
            video("live0000000", "Luis Fonsi - Despacito (Live)", "Luis Fonsi", 240),
            video("cover000000", "Despacito - Piano Cover", "Piano Guy", 229),
            video("nightcore00", "Despacito (Nightcore)", "NC", 180),
            video("other000000", "Daddy Yankee - Gasolina", "Daddy Yankee", 230),
            video(
                "kJQP7kiw5Fk",
                "Luis Fonsi - Despacito ft. Daddy Yankee",
                "LuisFonsiVEVO",
                282,
            ),
            video(
                "lyric000000",
                "Luis Fonsi, Daddy Yankee - Despacito (Letra / Lyrics)",
                "Lyrics Hub",
                229,
            ),
        ];
        let ids = confident_videos(&srcs, &vids);
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert!(ids.contains(&"kJQP7kiw5Fk".to_string()));
        assert!(ids.contains(&"lyric000000".to_string()));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    async fn store() -> (Store, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("lynx-ytm-{}.db", uuid::Uuid::new_v4()));
        (Store::open(&path).await.unwrap(), path)
    }

    async fn meta(yt: &YouTube, store: &Store, id: &str) -> (TrackInfo, Option<MusicMeta>) {
        let t = yt.video(id, false).await.unwrap();
        let m = lookup(yt, store, Duration::from_secs(60), &t).await.unwrap();
        (t, m)
    }

    #[tokio::test]
    #[ignore]
    async fn ytm_counterpart_live() {
        let yt = YouTube::new(reqwest::Client::new());
        let (store, path) = store().await;

        // Spanish-language official video -> its "Topic" song with the full artist list.
        let (t, m) = meta(&yt, &store, "wnJ6LuUFpMo").await;
        let m = m.expect("Mi Gente counterpart");
        println!("{} -> {m:?}", t.title);
        assert_ne!(m.youtube_id, "wnJ6LuUFpMo");
        assert_eq!(m.title, "Mi Gente");
        assert!(m.artists.iter().any(|a| a.contains("Balvin")) && m.artists.iter().any(|a| a == "Willy William"));

        // English official video (4K remaster upload).
        let (t, m) = meta(&yt, &store, "dQw4w9WgXcQ").await;
        let m = m.expect("Rick Astley counterpart");
        println!("{} -> {m:?}", t.title);
        assert_eq!(m.title, "Never Gonna Give You Up");
        assert_eq!(m.artists, vec!["Rick Astley"]);
        let song_id = m.youtube_id.clone();

        // The song itself stays as is.
        let (_, m) = meta(&yt, &store, &song_id).await;
        assert_eq!(m.map(|m| m.youtube_id).as_deref(), Some(song_id.as_str()));

        // Not music.
        let (_, m) = meta(&yt, &store, "jNQXAC9IVRw").await;
        assert!(m.is_none());
        // Negative results are cached.
        let cached = store
            .cache_get::<Option<MusicMeta>>(CACHE_NS, "jNQXAC9IVRw")
            .await
            .unwrap();
        assert_eq!(cached, Some(None));

        store.close().await;
        let _ = std::fs::remove_file(path);
    }
}
