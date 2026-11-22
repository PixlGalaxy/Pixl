//! Registry of every child process (yt-dlp, etc.), so none outlives its track or session.
//! The full implementation (process groups, timeouts, orphan sweep) lives in later phases;
//! the contract below is what the rest of the app relies on.

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

/// Who a child process belongs to. Killing by owner prefix (guild, or guild + channel) must be possible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProcOwner {
    pub guild_id: u64,
    pub voice_channel_id: u64,
    pub track_uid: Option<Uuid>,
}

#[derive(Clone, Default)]
pub struct ProcessRegistry {
    inner: Arc<DashMap<Uuid, (ProcOwner, u32)>>,
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Kill every registered process. Called on shutdown.
    pub async fn kill_all(&self) {
        self.inner.clear();
    }
}
