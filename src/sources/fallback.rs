//! Playback fallback: when a YouTube video cannot be played (region block, age gate, removed,
//! members-only...), play another upload of the same recording instead of failing.
//!
//! The decision logic is generic over [`Alternatives`] so it can be tested without the network.

use std::{collections::HashSet, future::Future};

use super::{SourceError, SourceResult, StreamInfo, TrackInfo, TrackOrigin};

pub const CACHE_NS: &str = "yt_fallback";
/// Alternative videos tried per track, on top of the original.
pub const MAX_ALTERNATIVES: usize = 2;

/// Errors that mean "this video will not play here", worth trying another upload. Timeouts,
/// network errors, bot checks and rate limits are not: another video would fail the same way.
pub fn fallback_worthy(e: &SourceError) -> bool {
    const NEEDLES: &[&str] = &[
        // yt-dlp reasons (see `ytdlp::short_reason`) and InnerTube `playabilityStatus.reason`.
        "age-restricted",
        "confirm your age",
        "inappropriate for some users",
        "private video",
        "video is private",
        "video unavailable",
        "video is unavailable",
        "video is not available",
        "not available in this country",
        "not available in your country",
        "not made this video available",
        "blocked it in your country",
        "copyright",
        "has been removed",
        "account associated with this video has been terminated",
        "no longer available",
        "members-only",
        "join this channel",
        "no audio stream",
    ];
    let SourceError::Unavailable(reason) = e else {
        return false;
    };
    let r = reason.to_lowercase();
    NEEDLES.iter().any(|n| r.contains(n))
}

/// Where an alternative comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The YouTube Music song (audio track) of a pasted video.
    MusicSong,
    /// A confident regular YouTube upload of the same recording.
    Video,
}

impl Step {
    pub fn label(self) -> &'static str {
        match self {
            Step::MusicSong => "YouTube Music song",
            Step::Video => "YouTube video",
        }
    }
}

/// Songs (YouTube Music results, Spotify matches) fall back to videos; pasted videos first to
/// their YouTube Music song, then to another video.
pub fn steps_for(track: &TrackInfo) -> &'static [Step] {
    if track.origin == TrackOrigin::Spotify || track.is_music_song() {
        &[Step::Video]
    } else {
        &[Step::MusicSong, Step::Video]
    }
}

pub trait Alternatives {
    /// Candidate video ids for one step, best first (already confident matches).
    fn candidates(&self, step: Step) -> impl Future<Output = SourceResult<Vec<String>>> + Send;
    fn try_stream(&self, id: &str) -> impl Future<Output = SourceResult<StreamInfo>> + Send;
}

#[derive(Debug)]
pub struct Found {
    pub id: String,
    pub stream: StreamInfo,
    pub step: Step,
}

#[derive(Debug)]
pub enum Outcome {
    Played(Box<Found>),
    /// Every alternative was searched and none plays: safe to cache.
    NoAlternative,
    /// A search or attempt failed for a transient reason: do not cache.
    Inconclusive,
}

/// Try up to `max` alternatives in step order, never an id in `tried` (which holds the original).
pub async fn try_alternatives<A: Alternatives + Sync>(
    alts: &A,
    steps: &[Step],
    mut tried: HashSet<String>,
    max: usize,
) -> Outcome {
    let mut attempts = 0;
    let mut inconclusive = false;
    'steps: for &step in steps {
        if attempts >= max {
            break;
        }
        let ids = match alts.candidates(step).await {
            Ok(ids) => ids,
            Err(_) => {
                inconclusive = true;
                continue;
            }
        };
        for id in ids {
            if attempts >= max {
                break 'steps;
            }
            if !tried.insert(id.clone()) {
                continue;
            }
            attempts += 1;
            match alts.try_stream(&id).await {
                Ok(stream) => return Outcome::Played(Box::new(Found { id, stream, step })),
                Err(SourceError::Cancelled) => return Outcome::Inconclusive,
                Err(e) if fallback_worthy(&e) => {}
                Err(_) => inconclusive = true,
            }
        }
    }
    if inconclusive {
        Outcome::Inconclusive
    } else {
        Outcome::NoAlternative
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use super::*;

    fn stream(id: &str) -> StreamInfo {
        StreamInfo {
            url: format!("https://cdn/{id}"),
            http_headers: Vec::new(),
            expires_at: i64::MAX,
            container: None,
            codec: None,
            format_id: None,
            protocol: None,
            is_opus: true,
            needs_ytdlp: false,
            duration_ms: None,
        }
    }

    /// Fake resolver: fixed candidates per step and a playability result per id.
    struct Fake {
        cands: HashMap<&'static str, SourceResult<Vec<String>>>,
        ok: HashSet<&'static str>,
        transient: HashSet<&'static str>,
        attempts: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(music: &[&str], videos: &[&str], ok: &[&'static str]) -> Self {
            let v = |l: &[&str]| Ok(l.iter().map(|s| s.to_string()).collect());
            Self {
                cands: HashMap::from([("music", v(music)), ("video", v(videos))]),
                ok: ok.iter().copied().collect(),
                transient: HashSet::new(),
                attempts: Mutex::new(Vec::new()),
            }
        }
        fn tried(&self) -> Vec<String> {
            self.attempts.lock().unwrap().clone()
        }
    }

    impl Alternatives for Fake {
        async fn candidates(&self, step: Step) -> SourceResult<Vec<String>> {
            let key = if step == Step::MusicSong { "music" } else { "video" };
            match &self.cands[key] {
                Ok(v) => Ok(v.clone()),
                Err(_) => Err(SourceError::Timeout),
            }
        }
        async fn try_stream(&self, id: &str) -> SourceResult<StreamInfo> {
            self.attempts.lock().unwrap().push(id.to_owned());
            if self.ok.contains(id) {
                Ok(stream(id))
            } else if self.transient.contains(id) {
                Err(SourceError::Timeout)
            } else {
                Err(SourceError::Unavailable("Not available in this country".into()))
            }
        }
    }

    fn orig() -> HashSet<String> {
        HashSet::from(["orig".to_string()])
    }

    const VIDEO_STEPS: &[Step] = &[Step::MusicSong, Step::Video];

    #[test]
    fn which_errors_trigger_fallback() {
        for r in [
            "Age-restricted video",
            "Private video",
            "Video unavailable",
            "Not available in this country",
            "Members-only video",
            "The uploader has not made this video available in your country",
            "Sign in to confirm your age",
            "This video contains content from X, who has blocked it in your country on copyright grounds",
            "No audio stream",
        ] {
            assert!(fallback_worthy(&SourceError::Unavailable(r.into())), "{r}");
        }
        for e in [
            SourceError::Timeout,
            SourceError::Cancelled,
            SourceError::Http("YouTube player returned 500".into()),
            SourceError::Unavailable("Sign in to confirm you're not a bot".into()),
            SourceError::Unavailable("Rate limited by the site (HTTP 429)".into()),
            SourceError::Unavailable("Access denied (HTTP 403)".into()),
            SourceError::Unavailable("Video has not premiered yet".into()),
            SourceError::NotFound("Video unavailable".into()),
        ] {
            assert!(!fallback_worthy(&e), "{e:?}");
        }
    }

    #[tokio::test]
    async fn music_song_first_then_video() {
        let f = Fake::new(&["atv"], &["vid1", "vid2"], &["atv", "vid1"]);
        let Outcome::Played(found) = try_alternatives(&f, VIDEO_STEPS, orig(), MAX_ALTERNATIVES).await else {
            panic!()
        };
        assert_eq!((found.id.as_str(), found.step), ("atv", Step::MusicSong));
        assert_eq!(f.tried(), vec!["atv"]);

        let f = Fake::new(&["atv"], &["vid1", "vid2"], &["vid1"]);
        let Outcome::Played(found) = try_alternatives(&f, VIDEO_STEPS, orig(), MAX_ALTERNATIVES).await else {
            panic!()
        };
        assert_eq!((found.id.as_str(), found.step), ("vid1", Step::Video));
    }

    #[tokio::test]
    async fn at_most_two_attempts_and_no_repeats() {
        // The video search returns the original, the song and duplicates: none is retried.
        let f = Fake::new(&["atv"], &["orig", "atv", "vid1", "vid1", "vid2", "vid3"], &["vid3"]);
        let out = try_alternatives(&f, VIDEO_STEPS, orig(), MAX_ALTERNATIVES).await;
        assert!(matches!(out, Outcome::NoAlternative), "{out:?}");
        assert_eq!(f.tried(), vec!["atv", "vid1"]);

        // Songs go straight to videos and get both attempts there.
        let f = Fake::new(&[], &["vid1", "vid2", "vid3"], &["vid2"]);
        let Outcome::Played(found) = try_alternatives(&f, &[Step::Video], orig(), MAX_ALTERNATIVES).await else {
            panic!()
        };
        assert_eq!(found.id, "vid2");
        assert_eq!(f.tried(), vec!["vid1", "vid2"]);

        // A previously cached substitute that failed is excluded too.
        let f = Fake::new(&["atv"], &["vid1"], &["vid1"]);
        let mut tried = orig();
        tried.insert("atv".into());
        let Outcome::Played(found) = try_alternatives(&f, VIDEO_STEPS, tried, MAX_ALTERNATIVES).await else {
            panic!()
        };
        assert_eq!(found.id, "vid1");
        assert_eq!(f.tried(), vec!["vid1"]);
    }

    #[tokio::test]
    async fn nothing_confident_means_no_alternative_and_transient_is_not_cached() {
        let f = Fake::new(&[], &[], &[]);
        assert!(matches!(
            try_alternatives(&f, VIDEO_STEPS, orig(), 2).await,
            Outcome::NoAlternative
        ));
        assert!(f.tried().is_empty());

        let mut f = Fake::new(&["atv"], &["vid1"], &[]);
        f.transient.insert("atv");
        assert!(matches!(
            try_alternatives(&f, VIDEO_STEPS, orig(), 2).await,
            Outcome::Inconclusive
        ));

        let mut f = Fake::new(&[], &[], &[]);
        f.cands.insert("video", Err(SourceError::Timeout));
        assert!(matches!(
            try_alternatives(&f, VIDEO_STEPS, orig(), 2).await,
            Outcome::Inconclusive
        ));
    }

    #[test]
    fn steps_depend_on_origin() {
        let mut t = crate::store::test_util::track("orig");
        assert_eq!(steps_for(&t), VIDEO_STEPS);
        t.origin = TrackOrigin::Spotify;
        assert_eq!(steps_for(&t), &[Step::Video]);
    }
}
