//! Track start-up: per-step timings of `Sources::prepare` and the YouTube Music metadata lookup
//! that runs alongside the stream resolve instead of before it.

use std::{
    fmt,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{sync::oneshot, time::Instant};

use super::MusicMeta;

/// Where the time of one track start went. Cheap: only `Instant`s and `Duration`s.
#[derive(Debug, Clone)]
pub struct PrepareTimings {
    /// When `prepare` was asked for.
    pub requested: Instant,
    /// Spotify / Apple Music -> YouTube match, when one ran.
    pub matched: Option<Duration>,
    pub meta: MetaTiming,
    /// Stream resolve (yt-dlp, or the cache), alternatives included, without the `available_at` wait.
    pub resolve: Duration,
    /// The stream URL came from the cache.
    pub cache_hit: bool,
    /// Time slept for `available_at`.
    pub waited: Duration,
    /// An alternative upload plays instead of the original video.
    pub fallback: bool,
    /// `requested` -> `prepare` returned.
    pub prepared: Duration,
    /// Prepared ahead of time while the previous track played.
    pub prefetch: bool,
}

impl PrepareTimings {
    pub fn new(requested: Instant) -> Self {
        Self {
            requested,
            matched: None,
            meta: MetaTiming::NotNeeded,
            resolve: Duration::ZERO,
            cache_hit: false,
            waited: Duration::ZERO,
            fallback: false,
            prepared: Duration::ZERO,
            prefetch: false,
        }
    }

    /// The one-line summary logged when the track is playable. `begun` is when the player
    /// started building the songbird input; `playable` when songbird reported it ready.
    pub fn summary(&self, begun: Instant, playable: Instant) -> StartupSummary<'_> {
        StartupSummary { t: self, begun, playable }
    }
}

/// How the YouTube Music metadata lookup went, as far as `prepare` saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaTiming {
    /// No lookup (already known, a catalog track, not YouTube).
    NotNeeded,
    /// Finished before `prepare` returned, after this long.
    Ready(Duration),
    /// Still running when the stream was ready: applied when it arrives.
    Late,
}

pub struct StartupSummary<'a> {
    t: &'a PrepareTimings,
    begun: Instant,
    playable: Instant,
}

impl fmt::Display for StartupSummary<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let t = self.t;
        let ms = |d: Duration| d.as_millis();
        match t.matched {
            Some(d) => write!(f, "match={}ms", ms(d))?,
            None => f.write_str("match=-")?,
        }
        match t.meta {
            MetaTiming::NotNeeded => f.write_str(" meta=-")?,
            MetaTiming::Ready(d) => write!(f, " meta={}ms", ms(d))?,
            MetaTiming::Late => f.write_str(" meta=late")?,
        }
        write!(
            f,
            " resolve={}ms{}{} wait={}ms prepare={}ms input={}ms",
            ms(t.resolve),
            if t.cache_hit { "(cached)" } else { "" },
            if t.fallback { "(fallback)" } else { "" },
            ms(t.waited),
            ms(t.prepared),
            ms(self.playable.saturating_duration_since(self.begun)),
        )?;
        // A prefetched track was asked to play when its input was built.
        let from = if t.prefetch { self.begun } else { t.requested };
        write!(f, " total={}ms", ms(self.playable.saturating_duration_since(from)))
    }
}

/// A YouTube Music metadata lookup running in its own task.
pub(crate) struct MetaLookup {
    rx: oneshot::Receiver<Option<MusicMeta>>,
    started: Instant,
}

/// What a lookup gave once the stream was ready.
pub(crate) enum MetaOutcome {
    Ready(Option<MusicMeta>, Duration),
    Late(LateMusic),
}

impl MetaLookup {
    /// Start `lookup` now. It keeps running (and fills the caches) even if nobody waits for it.
    pub fn spawn<F>(lookup: F) -> Self
    where
        F: Future<Output = Option<MusicMeta>> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = tx.send(lookup.await);
        });
        Self { rx, started: Instant::now() }
    }

    /// Wait for the result, however long the lookup takes (it is bounded by its own timeout).
    pub async fn wait(self) -> (Option<MusicMeta>, Duration) {
        let meta = self.rx.await.ok().flatten();
        (meta, self.started.elapsed())
    }

    /// Wait at most `grace` more; past that the result arrives later through [`LateMusic`].
    pub async fn settle(mut self, grace: Duration) -> MetaOutcome {
        match tokio::time::timeout(grace, &mut self.rx).await {
            Ok(r) => MetaOutcome::Ready(r.ok().flatten(), self.started.elapsed()),
            Err(_) => MetaOutcome::Late(LateMusic {
                rx: Arc::new(Mutex::new(Some(self.rx))),
                started: self.started,
            }),
        }
    }
}

/// YouTube Music metadata that was still being looked up when the track started. The player
/// takes it once (clones of a `Playable` share it) and applies it when it arrives.
#[derive(Clone)]
pub struct LateMusic {
    rx: Arc<Mutex<Option<oneshot::Receiver<Option<MusicMeta>>>>>,
    started: Instant,
}

impl LateMusic {
    /// The result, once. `None` from the future when nothing was found (or the lookup failed);
    /// the `Duration` is the lookup's total time. Later calls return `None`.
    pub fn take(&self) -> Option<impl Future<Output = (Option<MusicMeta>, Duration)> + Send + 'static> {
        let rx = self.rx.lock().unwrap_or_else(|p| p.into_inner()).take()?;
        let started = self.started;
        Some(async move { (rx.await.ok().flatten(), started.elapsed()) })
    }
}

impl fmt::Debug for LateMusic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LateMusic")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str) -> MusicMeta {
        MusicMeta {
            youtube_id: id.into(),
            title: "Song".into(),
            artists: vec!["Artist".into()],
            album: None,
            isrc: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fast_lookup_is_ready_within_the_grace() {
        let lookup = MetaLookup::spawn(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Some(meta("a"))
        });
        // The stream took longer than the lookup.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let start = Instant::now();
        match lookup.settle(Duration::from_millis(250)).await {
            MetaOutcome::Ready(m, took) => {
                assert_eq!(m, Some(meta("a")));
                assert!(took >= Duration::from_millis(50));
            }
            MetaOutcome::Late(_) => panic!("should be ready"),
        }
        assert_eq!(start.elapsed(), Duration::ZERO, "no extra wait when already done");
    }

    #[tokio::test(start_paused = true)]
    async fn slow_lookup_does_not_hold_up_the_start_and_arrives_later() {
        let lookup = MetaLookup::spawn(async {
            tokio::time::sleep(Duration::from_secs(8)).await;
            Some(meta("b"))
        });
        let start = Instant::now();
        let late = match lookup.settle(Duration::from_millis(250)).await {
            MetaOutcome::Late(l) => l,
            MetaOutcome::Ready(..) => panic!("should be late"),
        };
        assert_eq!(start.elapsed(), Duration::from_millis(250), "only the grace is waited");
        let shared = late.clone();
        let fut = late.take().expect("first take");
        assert!(shared.take().is_none(), "taken once across clones");
        let (m, took) = fut.await;
        assert_eq!(m, Some(meta("b")));
        assert!(took >= Duration::from_secs(8));
    }

    #[tokio::test(start_paused = true)]
    async fn failed_lookup_is_none() {
        let lookup = MetaLookup::spawn(async { None });
        let (m, _) = lookup.wait().await;
        assert!(m.is_none());
        let lookup = MetaLookup::spawn(async { panic!("lookup task died") });
        match lookup.settle(Duration::from_millis(10)).await {
            MetaOutcome::Ready(m, _) => assert!(m.is_none()),
            MetaOutcome::Late(l) => assert!(l.take().unwrap().await.0.is_none()),
        }
    }

    #[test]
    fn summary_line() {
        let t0 = Instant::now();
        let mut t = PrepareTimings::new(t0);
        t.matched = Some(Duration::from_millis(420));
        t.meta = MetaTiming::Late;
        t.resolve = Duration::from_millis(2100);
        t.waited = Duration::from_millis(1300);
        t.prepared = Duration::from_millis(3900);
        let begun = t0 + Duration::from_millis(3910);
        let playable = t0 + Duration::from_millis(4500);
        assert_eq!(
            t.summary(begun, playable).to_string(),
            "match=420ms meta=late resolve=2100ms wait=1300ms prepare=3900ms input=590ms total=4500ms"
        );
        t.matched = None;
        t.meta = MetaTiming::Ready(Duration::from_millis(80));
        t.cache_hit = true;
        t.prefetch = true;
        assert_eq!(
            t.summary(begun, playable).to_string(),
            "match=- meta=80ms resolve=2100ms(cached) wait=1300ms prepare=3900ms input=590ms total=590ms"
        );
    }
}
