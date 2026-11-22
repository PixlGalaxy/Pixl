//! Turning user input (URLs, search text, Spotify links) into playable tracks.

use serde::{Deserialize, Serialize};

/// Where a track was originally requested from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackOrigin {
    Youtube,
    YoutubeMusic,
    Spotify,
    Other,
}

/// Metadata for one queue entry. Spotify entries may not have a YouTube match yet (`youtube_id == None`):
/// they are resolved lazily shortly before they play.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackInfo {
    pub title: String,
    pub artists: Vec<String>,
    pub duration_ms: Option<u64>,
    pub thumbnail: Option<String>,
    /// Canonical page URL shown to users.
    pub url: String,
    pub origin: TrackOrigin,
    pub youtube_id: Option<String>,
    pub spotify_id: Option<String>,
    pub isrc: Option<String>,
}
