//! Lists (playlists, albums, artist pages) for the saved-playlists feature: which inputs are lists,
//! their canonical URL and platform, and the shared track cache.
//!
//! A list's tracks are cached for [`LIST_TTL`] per canonical URL in the store's `cache` table, so
//! every user and guild shares them: ten people opening the same Spotify playlist cost one fetch.
//! Concurrent misses for the same URL wait for one fetch (single flight) instead of each going
//! upstream.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};

use super::{detect::Input, SourceResult, TrackInfo};
use crate::store::Store;

pub const CACHE_NS: &str = "list";
/// A list is never served older than this.
pub const LIST_TTL: Duration = Duration::from_secs(60 * 60);

/// Where a saved list lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Youtube,
    YoutubeMusic,
    Spotify,
    AppleMusic,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Youtube => "youtube",
            Self::YoutubeMusic => "youtube_music",
            Self::Spotify => "spotify",
            Self::AppleMusic => "apple_music",
        }
    }
}

/// A list input reduced to one canonical URL (tracking parameters, the clicked video, locale
/// prefixes and short links are gone), so the same list is one cache entry and one saved row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRef {
    pub url: String,
    pub platform: Platform,
}

impl ListRef {
    /// The list a stored canonical URL points to (no network).
    pub fn parse(url: &str) -> Option<Self> {
        list_ref(&super::detect::detect(url))
    }
}

/// The canonical list behind an input, or `None` for single tracks, searches and other URLs.
pub fn list_ref(input: &Input) -> Option<ListRef> {
    let (url, platform) = match input {
        Input::YoutubePlaylist { id, music: false } => {
            (format!("https://www.youtube.com/playlist?list={id}"), Platform::Youtube)
        }
        Input::YoutubePlaylist { id, music: true } => (
            format!("https://music.youtube.com/playlist?list={id}"),
            Platform::YoutubeMusic,
        ),
        Input::YoutubeMusicAlbum(id) => (format!("https://music.youtube.com/browse/{id}"), Platform::YoutubeMusic),
        Input::YoutubeMusicArtist(id) => (
            format!("https://music.youtube.com/channel/{id}"),
            Platform::YoutubeMusic,
        ),
        Input::SpotifyAlbum(id) => (format!("https://open.spotify.com/album/{id}"), Platform::Spotify),
        Input::SpotifyPlaylist(id) => (format!("https://open.spotify.com/playlist/{id}"), Platform::Spotify),
        Input::AppleAlbum { country, id } => (
            format!("https://music.apple.com/{country}/album/{id}"),
            Platform::AppleMusic,
        ),
        Input::ApplePlaylist { country, id } => (
            format!("https://music.apple.com/{country}/playlist/{id}"),
            Platform::AppleMusic,
        ),
        _ => return None,
    };
    Some(ListRef { url, platform })
}

/// One cached fetch of a list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedList {
    pub name: Option<String>,
    /// Cover (the platform's, else the first track's artwork).
    pub image: Option<String>,
    pub tracks: Vec<TrackInfo>,
    /// Unix seconds of the fetch.
    pub fetched_at: i64,
}

/// Shared, TTL-bound list cache with single-flight fetches.
pub struct ListCache {
    store: Store,
    ttl: Duration,
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ListCache {
    pub fn new(store: Store, ttl: Duration) -> Self {
        Self {
            store,
            ttl,
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// The cached list when it is still fresh. Never fetches.
    pub async fn peek(&self, url: &str) -> Option<CachedList> {
        self.store.cache_get(CACHE_NS, url).await.ok().flatten()
    }

    /// The fresh cached list, or `fetch` it (and cache it). `force` skips the cache. Callers for the
    /// same URL share one fetch: whoever waits finds the cache filled when its turn comes.
    pub async fn get_or_fetch<F, Fut>(&self, url: &str, force: bool, fetch: F) -> SourceResult<CachedList>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = SourceResult<CachedList>>,
    {
        if !force {
            if let Some(hit) = self.peek(url).await {
                return Ok(hit);
            }
        }
        let gate = self.inflight.lock().unwrap().entry(url.to_owned()).or_default().clone();
        let out = {
            let _turn = gate.lock().await;
            match self.peek(url).await {
                // Someone else fetched it while this caller waited.
                Some(hit) if !force => Ok(hit),
                _ => match fetch().await {
                    Ok(list) => {
                        let _ = self.store.cache_put(CACHE_NS, url, &list, self.ttl).await;
                        Ok(list)
                    }
                    Err(e) => Err(e),
                },
            }
        };
        let mut map = self.inflight.lock().unwrap();
        // Map + this caller: nobody else is waiting on the gate.
        if map.get(url).is_some_and(|g| Arc::strong_count(g) <= 2) {
            map.remove(url);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::sources::{detect::detect, SourceError};

    fn list(name: &str) -> CachedList {
        CachedList {
            name: Some(name.into()),
            image: None,
            tracks: vec![crate::store::test_util::track("a")],
            fetched_at: 0,
        }
    }

    #[test]
    fn canonical_urls_round_trip_and_drop_noise() {
        let cases = [
            (
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI&index=3",
                "https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI",
                Platform::Youtube,
            ),
            (
                "https://music.youtube.com/playlist?list=OLAK5uy_kq0yXm5W2-Bf3Lr0b8YRZ9b2b3Q_Y8rXo&si=x",
                "https://music.youtube.com/playlist?list=OLAK5uy_kq0yXm5W2-Bf3Lr0b8YRZ9b2b3Q_Y8rXo",
                Platform::YoutubeMusic,
            ),
            (
                "music.youtube.com/browse/MPREb_NMmY1n1NJFS?feature=share",
                "https://music.youtube.com/browse/MPREb_NMmY1n1NJFS",
                Platform::YoutubeMusic,
            ),
            (
                "https://music.youtube.com/channel/UCk91oFc2hY2CdHirU93baLg?si=x",
                "https://music.youtube.com/channel/UCk91oFc2hY2CdHirU93baLg",
                Platform::YoutubeMusic,
            ),
            (
                "https://open.spotify.com/intl-es/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abc",
                "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
                Platform::Spotify,
            ),
            (
                "spotify:album:4aawyAB9vmqN3uQ7FjRGTy",
                "https://open.spotify.com/album/4aawyAB9vmqN3uQ7FjRGTy",
                Platform::Spotify,
            ),
            (
                "https://music.apple.com/us/album/un-verano-sin-ti/1622045624?uo=4",
                "https://music.apple.com/us/album/1622045624",
                Platform::AppleMusic,
            ),
            (
                "https://music.apple.com/mx/playlist/top-100/pl.d25f5d1181894928af76c85c967f8f31",
                "https://music.apple.com/mx/playlist/pl.d25f5d1181894928af76c85c967f8f31",
                Platform::AppleMusic,
            ),
        ];
        for (input, url, platform) in cases {
            let r = list_ref(&detect(input)).unwrap_or_else(|| panic!("{input} is a list"));
            assert_eq!((r.url.as_str(), r.platform), (url, platform), "{input}");
            // The canonical URL is the same list again.
            assert_eq!(list_ref(&detect(&r.url)), Some(r.clone()), "{url}");
        }
        for single in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://open.spotify.com/track/4PTG3Z6ehGkBFwjybzWkR8",
            "https://music.apple.com/us/album/moscow-mule/1622045624?i=1622045625",
            "bad bunny",
            "https://soundcloud.com/a/sets/b",
        ] {
            assert!(list_ref(&detect(single)).is_none(), "{single}");
        }
    }

    #[tokio::test]
    async fn cache_hits_until_the_ttl_and_force_refetches() {
        let store = Store::open_in_memory().await.unwrap();
        let cache = ListCache::new(store.clone(), LIST_TTL);
        let fetches = AtomicUsize::new(0);
        let fetch = |name: &'static str| {
            let fetches = &fetches;
            move || async move {
                fetches.fetch_add(1, Ordering::SeqCst);
                Ok(list(name))
            }
        };
        let a = cache.get_or_fetch("u", false, fetch("one")).await.unwrap();
        let b = cache.get_or_fetch("u", false, fetch("two")).await.unwrap();
        assert_eq!((a.name.as_deref(), b.name.as_deref()), (Some("one"), Some("one")));
        assert_eq!(fetches.load(Ordering::SeqCst), 1, "second open is a cache hit");
        let c = cache.get_or_fetch("u", true, fetch("three")).await.unwrap();
        assert_eq!(c.name.as_deref(), Some("three"));
        assert_eq!(
            cache.peek("u").await.unwrap().name.as_deref(),
            Some("three"),
            "refresh replaces the cache"
        );
        assert_eq!(fetches.load(Ordering::SeqCst), 2);

        // Expired entries are misses (a zero TTL expires right away).
        let stale = ListCache::new(store, Duration::ZERO);
        stale.get_or_fetch("v", false, fetch("x")).await.unwrap();
        assert!(stale.peek("v").await.is_none());
        stale.get_or_fetch("v", false, fetch("y")).await.unwrap();
        assert_eq!(fetches.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn failures_are_not_cached_and_concurrent_misses_share_one_fetch() {
        let store = Store::open_in_memory().await.unwrap();
        let cache = Arc::new(ListCache::new(store, LIST_TTL));
        let err = cache
            .get_or_fetch("u", false, || async { Err(SourceError::Http("down".into())) })
            .await;
        assert!(err.is_err());
        assert!(cache.peek("u").await.is_none());

        let fetches = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let (cache, fetches) = (cache.clone(), fetches.clone());
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_fetch("u", false, || async move {
                        fetches.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(list("shared"))
                    })
                    .await
                    .unwrap()
            }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap().name.as_deref(), Some("shared"));
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 1, "ten users, one upstream fetch");
        assert!(cache.inflight.lock().unwrap().is_empty(), "gates are cleaned up");
    }
}
