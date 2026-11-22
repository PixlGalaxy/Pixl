//! Spotify -> YouTube matching: ISRC search on YouTube Music, then "artist title" song search,
//! then plain video search, each candidate scored against the Spotify metadata.

use std::{collections::HashSet, time::Duration};

use serde::{Deserialize, Serialize};
use tracing::debug;

use super::{
    text::{contains_phrase, normalize, tokens},
    youtube::YouTube,
    SourceError, SourceResult, TrackInfo, TrackOrigin,
};
use crate::store::Store;

pub const CACHE_NS: &str = "yt_match";
/// Minimum score to accept a candidate at all.
pub const ACCEPT: i32 = 45;
/// Score that ends the search early (no need for the text search).
pub const CONFIDENT: i32 = 85;

/// Words that mark a different recording when the source title does not have them.
const VARIANTS: &[&str] = &[
    "live", "en vivo", "ao vivo", "en directo", "cover", "remix", "sped up", "speed up", "slowed", "reverb",
    "nightcore", "8d", "karaoke", "instrumental", "acoustic", "acustico", "unplugged", "mashup", "bass boosted",
    "extended", "tribute", "reaction", "1 hour", "loop", "piano version", "demo", "chipmunk", "daycore",
];

/// Bracketed or dashed segments that carry no identity ("Official Video", "Remastered 2011").
const NOISE: &[&str] = &[
    "official", "video", "audio", "music", "lyric", "lyrics", "letra", "oficial", "visualizer", "visualiser",
    "hd", "hq", "4k", "remaster", "remastered", "version", "mv", "clip", "videoclip", "explicit", "clean",
    "radio", "edit", "original", "mix", "single", "album", "from", "the", "en", "con", "with", "lyricvideo",
    "remasterizado", "remasterizada", "stereo", "mono", "full", "color", "coded", "sub", "subtitulado",
];

#[derive(Debug, Clone)]
pub struct Candidate {
    pub track: TrackInfo,
    /// Uploader channel as shown on YouTube (keeps the " - Topic" suffix).
    pub channel: Option<String>,
    pub from_isrc: bool,
}

impl Candidate {
    pub fn new(track: TrackInfo) -> Self {
        Self { track, channel: None, from_isrc: false }
    }
}

/// Remove "feat." credits and noise brackets; keep meaningful brackets ("Remix", "Live").
pub fn clean_title(raw: &str) -> String {
    let mut kept = String::new();
    let mut depth = 0usize;
    let mut group = String::new();
    for c in raw.chars() {
        match c {
            '(' | '[' | '{' => {
                if depth == 0 {
                    group.clear();
                } else {
                    group.push(' ');
                }
                depth += 1;
            }
            ')' | ']' | '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 && !is_noise_or_credit(&group) {
                    kept.push(' ');
                    kept.push_str(&group);
                    kept.push(' ');
                }
            }
            _ if depth > 0 => group.push(c),
            _ => kept.push(c),
        }
    }
    // Spotify style suffixes: "Song - Remastered 2011", "Song - Live at X". Keep non-noise parts.
    let mut parts = kept.split(" - ");
    let head = parts.next().unwrap_or("").to_owned();
    let mut out = strip_inline_credit(&head);
    for p in parts {
        if !is_noise_or_credit(p) {
            out.push(' ');
            out.push_str(&strip_inline_credit(p));
        }
    }
    normalize(&out)
}

fn strip_inline_credit(s: &str) -> String {
    let n = format!(" {} ", s.to_lowercase());
    for marker in [" feat. ", " feat ", " ft. ", " ft ", " featuring "] {
        if let Some(pos) = n.find(marker) {
            // pos indexes the padded lowercase string; map back (lowercasing can change length,
            // so cut on a char boundary of the original by counting chars).
            let chars_before = n[..pos].chars().count().saturating_sub(1);
            return s.chars().take(chars_before).collect();
        }
    }
    s.to_owned()
}

fn is_noise_or_credit(group: &str) -> bool {
    let t = tokens(group);
    if t.is_empty() {
        return true;
    }
    if matches!(t[0].as_str(), "feat" | "ft" | "featuring" | "with" | "prod" | "con") {
        return true;
    }
    t.iter().all(|w| NOISE.contains(&w.as_str()) || w.chars().all(|c| c.is_ascii_digit()))
}

fn variants_in(normalized: &str) -> HashSet<&'static str> {
    VARIANTS.iter().copied().filter(|v| contains_phrase(normalized, v)).collect()
}

fn dice(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let common = a.intersection(b).count() as f64;
    2.0 * common / (a.len() + b.len()) as f64
}

/// Score one candidate. Higher is better; see `ACCEPT`.
pub fn score(src: &TrackInfo, cand: &Candidate) -> i32 {
    let c = &cand.track;
    let mut s = 0i32;

    // Duration.
    if let (Some(a), Some(b)) = (src.duration_ms, c.duration_ms) {
        let d = a.abs_diff(b);
        s += match d {
            0..=3_000 => 35,
            3_001..=10_000 => 15,
            10_001..=20_000 => 0,
            20_001..=60_000 => -20,
            _ => -50,
        };
    }

    // Artists: candidate artists, channel and (for "Artist - Title" uploads) the title itself.
    let src_artists: Vec<String> = src.artists.iter().map(|a| normalize(a)).filter(|a| !a.is_empty()).collect();
    let channel_raw = cand.channel.as_deref().unwrap_or("");
    let is_topic = channel_raw.trim_end().ends_with("- Topic") || c.origin == TrackOrigin::YoutubeMusic;
    let cand_artist_text = normalize(&format!("{} {}", c.artists.join(" "), channel_raw));
    let cand_title_norm = normalize(&c.title);
    let artist_hit = |a: &str| contains_phrase(&cand_artist_text, a) || contains_phrase(&cand_title_norm, a);
    match src_artists.first() {
        Some(primary) if artist_hit(primary) => {
            s += 25;
            s += src_artists.iter().skip(1).filter(|a| artist_hit(a)).count().min(2) as i32 * 3;
        }
        Some(_) if src_artists.iter().skip(1).any(|a| artist_hit(a)) => s += 12,
        Some(_) => s -= 15,
        None => {}
    }
    if is_topic {
        s += 10;
    }

    // Title similarity on cleaned tokens. Artist names in the candidate title ("Artist - Song")
    // are ignored unless the source title contains them too.
    let src_clean: HashSet<String> = clean_title(&src.title).split(' ').filter(|t| !t.is_empty()).map(str::to_owned).collect();
    let artist_tokens: HashSet<String> = src_artists.iter().flat_map(|a| a.split(' ').map(str::to_owned)).collect();
    let cand_clean: HashSet<String> = clean_title(&c.title)
        .split(' ')
        .filter(|t| !t.is_empty())
        .filter(|t| src_clean.contains(*t) || !artist_tokens.contains(*t))
        .map(str::to_owned)
        .collect();
    let sim = dice(&src_clean, &cand_clean);
    s += (sim * 40.0).round() as i32;
    if sim < 0.3 {
        s -= 20;
    }

    // Different recordings: penalize variants the source does not have, and missing ones it has.
    let src_var = variants_in(&normalize(&src.title));
    let cand_var = variants_in(&cand_title_norm);
    s -= 45 * cand_var.difference(&src_var).count().min(2) as i32;
    s -= 20 * src_var.difference(&cand_var).count().min(2) as i32;

    if cand.from_isrc {
        s += 15;
    }
    s
}

/// Best candidate above `ACCEPT`, with its score.
pub fn best<'a>(src: &TrackInfo, cands: &'a [Candidate]) -> Option<(&'a Candidate, i32)> {
    cands
        .iter()
        .map(|c| (c, score(src, c)))
        .max_by_key(|(_, s)| *s)
        .filter(|(_, s)| *s >= ACCEPT)
}

/// The text query used for the song search.
pub fn query_for(src: &TrackInfo) -> String {
    // Drop "(feat. X)" / "(with X)" groups but keep the original casing and other brackets.
    let mut title = String::new();
    let mut rest = src.title.as_str();
    while let Some(open) = rest.find(['(', '[']) {
        let close = rest[open..].find([')', ']']).map(|c| open + c);
        let Some(close) = close else { break };
        let inner = &rest[open + 1..close];
        title.push_str(&rest[..open]);
        let first = tokens(inner).into_iter().next().unwrap_or_default();
        if !matches!(first.as_str(), "feat" | "ft" | "featuring" | "with" | "con") {
            title.push_str(&rest[open..=close]);
        }
        rest = &rest[close + 1..];
    }
    title.push_str(rest);
    let title = strip_inline_credit(title.trim()).split_whitespace().collect::<Vec<_>>().join(" ");
    match src.artists.first() {
        Some(a) => format!("{a} {title}"),
        None => title,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedMatch {
    video_id: String,
    title: String,
    duration_ms: Option<u64>,
}

pub fn cache_key(src: &TrackInfo) -> Option<String> {
    src.isrc
        .as_ref()
        .map(|i| format!("isrc:{}", i.to_ascii_uppercase()))
        .or_else(|| src.spotify_id.as_ref().map(|i| format!("sp:{i}")))
}

/// Find the YouTube video for a Spotify track. Returns the source track with `youtube_id` set.
pub async fn find(yt: &YouTube, store: &Store, ttl: Duration, src: &TrackInfo) -> SourceResult<TrackInfo> {
    let key = cache_key(src);
    if let Some(k) = &key {
        if let Ok(Some(hit)) = store.cache_get::<CachedMatch>(CACHE_NS, k).await {
            return Ok(with_video(src, &hit.video_id));
        }
    }

    let mut cands: Vec<Candidate> = Vec::new();
    if let Some(isrc) = &src.isrc {
        match yt.search_songs(isrc, 5).await {
            Ok(r) => cands.extend(r.into_iter().map(|t| Candidate { track: t, channel: None, from_isrc: true })),
            Err(e) => debug!(error = %e, "ISRC search failed"),
        }
    }
    let confident = best(src, &cands).is_some_and(|(_, s)| s >= CONFIDENT);
    if !confident {
        let q = query_for(src);
        cands.extend(yt.search_songs(&q, 10).await?.into_iter().map(Candidate::new));
        if best(src, &cands).is_none() {
            // Not on YouTube Music as a song: fall back to regular videos.
            if let Ok(r) = yt.search_videos(&q, 8).await {
                cands.extend(r.into_iter().map(|t| {
                    let channel = t.artists.first().cloned();
                    Candidate { track: t, channel, from_isrc: false }
                }));
            }
        }
    }
    let Some((winner, sc)) = best(src, &cands) else {
        return Err(SourceError::NotFound(format!("No YouTube match for \"{}\"", query_for(src))));
    };
    let vid = winner.track.youtube_id.clone().ok_or_else(|| SourceError::Other("candidate without id".into()))?;
    debug!(score = sc, title = %winner.track.title, %vid, "spotify match");
    if let Some(k) = &key {
        let entry = CachedMatch { video_id: vid.clone(), title: winner.track.title.clone(), duration_ms: winner.track.duration_ms };
        let _ = store.cache_put(CACHE_NS, k, &entry, ttl).await;
    }
    Ok(with_video(src, &vid))
}

fn with_video(src: &TrackInfo, vid: &str) -> TrackInfo {
    let mut t = src.clone();
    t.youtube_id = Some(vid.to_owned());
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(title: &str, artists: &[&str], secs: u64) -> TrackInfo {
        TrackInfo {
            title: title.into(),
            artists: artists.iter().map(|s| s.to_string()).collect(),
            duration_ms: Some(secs * 1000),
            thumbnail: None,
            url: String::new(),
            origin: TrackOrigin::Spotify,
            youtube_id: None,
            spotify_id: Some("sp".into()),
            isrc: None,
        }
    }

    fn song(id: &str, title: &str, artists: &[&str], secs: u64) -> Candidate {
        let mut t = src(title, artists, secs);
        t.origin = TrackOrigin::YoutubeMusic;
        t.youtube_id = Some(id.into());
        Candidate::new(t)
    }

    fn video(id: &str, title: &str, channel: &str, secs: u64) -> Candidate {
        let mut t = src(title, &[channel.trim_end_matches(" - Topic")], secs);
        t.origin = TrackOrigin::Youtube;
        t.youtube_id = Some(id.into());
        Candidate { track: t, channel: Some(channel.into()), from_isrc: false }
    }

    fn pick(s: &TrackInfo, c: &[Candidate]) -> Option<String> {
        best(s, c).and_then(|(c, _)| c.track.youtube_id.clone())
    }

    #[test]
    fn clean_title_strips_credits_and_noise() {
        assert_eq!(clean_title("Me Porto Bonito (feat. Chencho Corleone)"), "me porto bonito");
        assert_eq!(clean_title("Song ft. Someone Else"), "song");
        assert_eq!(clean_title("Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)"), "rick astley never gonna give you up");
        assert_eq!(clean_title("Here Comes The Sun - Remastered 2009"), "here comes the sun");
        assert_eq!(clean_title("Song (Remix)"), "song remix");
        assert_eq!(clean_title("Song - Live at Wembley"), "song live at wembley");
        assert_eq!(clean_title("Tití Me Preguntó [Video Oficial]"), "titi me pregunto");
        assert_eq!(clean_title("Canción (con Otro Artista)"), "cancion");
        assert_eq!(clean_title("Lose Yourself [From \"8 Mile\"]"), "lose yourself from 8 mile");
    }

    #[test]
    fn prefers_original_over_remix_and_sped_up() {
        let s = src("Blinding Lights", &["The Weeknd"], 200);
        let c = [
            song("remix000000", "Blinding Lights (Major Lazer Remix)", &["The Weeknd", "Major Lazer"], 201),
            song("spedup00000", "Blinding Lights (Sped Up)", &["The Weeknd"], 160),
            song("orig0000000", "Blinding Lights", &["The Weeknd"], 200),
            song("nightcore00", "Blinding Lights - Nightcore", &["Nightcore Bros"], 170),
        ];
        assert_eq!(pick(&s, &c).as_deref(), Some("orig0000000"));
    }

    #[test]
    fn picks_remix_when_the_source_is_the_remix() {
        let s = src("Blinding Lights - Major Lazer Remix", &["The Weeknd", "Major Lazer"], 201);
        let c = [
            song("orig0000000", "Blinding Lights", &["The Weeknd"], 200),
            song("remix000000", "Blinding Lights (Major Lazer Remix)", &["The Weeknd", "Major Lazer"], 201),
        ];
        assert_eq!(pick(&s, &c).as_deref(), Some("remix000000"));
    }

    #[test]
    fn prefers_studio_over_live() {
        let s = src("Bohemian Rhapsody", &["Queen"], 354);
        let c = [
            song("live0000000", "Bohemian Rhapsody (Live Aid 1985)", &["Queen"], 356),
            video("liveyt00000", "Queen - Bohemian Rhapsody (Live at Wembley '86)", "Queen Official", 354),
            song("studio00000", "Bohemian Rhapsody (Remastered 2011)", &["Queen"], 355),
        ];
        assert_eq!(pick(&s, &c).as_deref(), Some("studio00000"));
    }

    #[test]
    fn picks_live_when_the_source_is_live() {
        let s = src("Bohemian Rhapsody - Live Aid", &["Queen"], 356);
        let c = [
            song("studio00000", "Bohemian Rhapsody", &["Queen"], 354),
            song("live0000000", "Bohemian Rhapsody (Live Aid)", &["Queen"], 357),
        ];
        assert_eq!(pick(&s, &c).as_deref(), Some("live0000000"));
    }

    #[test]
    fn feat_variations_match() {
        let s = src("Me Porto Bonito (feat. Chencho Corleone)", &["Bad Bunny", "Chencho Corleone"], 178);
        let c = [
            song("karaoke0000", "Me Porto Bonito (Karaoke Version)", &["Karaoke Hits"], 178),
            song("right000000", "Me Porto Bonito", &["Bad Bunny", "Chencho Corleone"], 179),
            video("video000000", "BAD BUNNY ft. CHENCHO CORLEONE - ME PORTO BONITO (Video Oficial)", "Bad Bunny", 190),
        ];
        assert_eq!(pick(&s, &c).as_deref(), Some("right000000"));

        // Spotify lists the featured artist only in the title; YouTube lists both as artists.
        let s = src("Taki Taki (with Selena Gomez, Ozuna & Cardi B)", &["DJ Snake"], 213);
        let c = [song("taki0000000", "Taki Taki (feat. Selena Gomez, Ozuna & Cardi B)", &["DJ Snake", "Selena Gomez"], 213)];
        assert_eq!(pick(&s, &c).as_deref(), Some("taki0000000"));
    }

    #[test]
    fn accented_spanish_titles_match() {
        let s = src("Tití Me Preguntó", &["Bad Bunny"], 243);
        let c = [
            song("cover000000", "Titi Me Pregunto (Soul Cover)", &["soulgenic"], 187),
            song("karaoke0000", "Tití Me Preguntó (By Bad Bunny) (Instrumental Karaoke Version)", &["ZZang KARAOKE"], 242),
            song("other000000", "Tití Me Preguntó", &["DJ Zaro", "Vandal On Da Track"], 108),
            song("juRFjpB5Ppg", "Titi Me Pregunto", &["Bad Bunny"], 244),
        ];
        assert_eq!(pick(&s, &c).as_deref(), Some("juRFjpB5Ppg"));

        let s = src("Despacito", &["Luis Fonsi", "Daddy Yankee"], 229);
        let c = [
            video("kJQP7kiw5Fk", "Luis Fonsi - Despacito ft. Daddy Yankee", "Luis Fonsi", 282),
            video("topic000000", "Despacito", "Luis Fonsi - Topic", 229),
        ];
        assert_eq!(pick(&s, &c).as_deref(), Some("topic000000"));
    }

    #[test]
    fn music_video_with_long_intro_still_matches_when_nothing_better() {
        let s = src("Despacito", &["Luis Fonsi", "Daddy Yankee"], 229);
        let c = [video("kJQP7kiw5Fk", "Luis Fonsi - Despacito ft. Daddy Yankee", "Luis Fonsi", 282)];
        // 53 s off: weak, but artist + exact title still clear the bar.
        assert_eq!(pick(&s, &c).as_deref(), Some("kJQP7kiw5Fk"));
    }

    #[test]
    fn rejects_unrelated_isrc_garbage() {
        let s = src("Never Gonna Give You Up", &["Rick Astley"], 213);
        let mut junk = song("junk0000000", "Maasi Maasam", &["Ilaiyaraaja", "K.J. Yesudas"], 299);
        junk.from_isrc = true;
        assert!(best(&s, &[junk.clone()]).is_none());
        let mut good = song("dQw4w9WgXcQ", "Never Gonna Give You Up", &["Rick Astley"], 214);
        good.from_isrc = true;
        let both = [junk, good];
        let (c, sc) = best(&s, &both).unwrap();
        assert_eq!(c.track.youtube_id.as_deref(), Some("dQw4w9WgXcQ"));
        assert!(sc >= CONFIDENT, "score {sc}");
    }

    #[test]
    fn same_artist_different_song_is_rejected() {
        let s = src("Yonaguni", &["Bad Bunny"], 207);
        let c = [song("x0000000000", "Un Coco", &["Bad Bunny"], 197)];
        assert!(best(&s, &c).is_none());
    }

    #[test]
    fn source_title_containing_variant_word_is_not_penalized() {
        // "Live" is part of the actual song name.
        let s = src("Live Forever", &["Oasis"], 276);
        let c = [song("a0000000000", "Live Forever (Remastered)", &["Oasis"], 277)];
        assert!(best(&s, &c).is_some());
        // "Oliver" must not trigger the "live" variant.
        assert!(variants_in(&normalize("Oliver Twist")).is_empty());
    }

    #[test]
    fn query_and_cache_key() {
        let mut s = src("Me Porto Bonito (feat. Chencho Corleone)", &["Bad Bunny"], 178);
        assert_eq!(query_for(&s), "Bad Bunny Me Porto Bonito");
        assert_eq!(cache_key(&s).as_deref(), Some("sp:sp"));
        s.isrc = Some("qm6mz2214990".into());
        assert_eq!(cache_key(&s).as_deref(), Some("isrc:QM6MZ2214990"));
    }

    // ---- network

    #[tokio::test]
    #[ignore]
    async fn matcher_live_isrc_and_text() {
        let yt = YouTube::new(reqwest::Client::new());
        let path = std::env::temp_dir().join(format!("lynx-match-{}.db", uuid::Uuid::new_v4()));
        let store = Store::open(&path).await.unwrap();
        let mut s = src("Never Gonna Give You Up", &["Rick Astley"], 213);
        s.isrc = Some("GBARL9300135".into());
        let m = find(&yt, &store, Duration::from_secs(60), &s).await.unwrap();
        println!("isrc match: {:?}", m.youtube_id);
        assert!(m.youtube_id.is_some());
        // Second lookup comes from the SQLite cache.
        let again = find(&yt, &store, Duration::from_secs(60), &s).await.unwrap();
        assert_eq!(again.youtube_id, m.youtube_id);

        let s = src("Tití Me Preguntó", &["Bad Bunny"], 243);
        let m = find(&yt, &store, Duration::from_secs(60), &s).await.unwrap();
        println!("text match: {:?}", m.youtube_id);
        assert!(m.youtube_id.is_some());
        store.close().await;
        let _ = std::fs::remove_file(path);
    }
}
