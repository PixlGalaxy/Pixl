//! Per-channel temporary directories for stream fragments:
//! `DATA_DIR/streams/{guild_id}/{voice_channel_id}/{track_uid}/`.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use uuid::Uuid;

use super::ProcOwner;

#[derive(Debug, Clone)]
pub struct StreamDirs {
    root: PathBuf,
    max_bytes: u64,
}

impl StreamDirs {
    pub fn new(root: impl Into<PathBuf>, max_mb: u64) -> Self {
        Self {
            root: root.into(),
            max_bytes: max_mb.saturating_mul(1024 * 1024),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn channel_path(&self, guild_id: u64, voice_channel_id: u64) -> PathBuf {
        self.root.join(guild_id.to_string()).join(voice_channel_id.to_string())
    }

    pub fn track_path(&self, guild_id: u64, voice_channel_id: u64, track_uid: Uuid) -> PathBuf {
        self.channel_path(guild_id, voice_channel_id)
            .join(track_uid.to_string())
    }

    /// Create (if needed) and return the work dir for `owner`. Owners without a track get a
    /// fresh one-off dir that the caller should remove when done.
    pub async fn create_for(&self, owner: &ProcOwner) -> std::io::Result<PathBuf> {
        let uid = owner.track_uid.unwrap_or_else(Uuid::new_v4);
        let dir = self.track_path(owner.guild_id, owner.voice_channel_id, uid);
        tokio::fs::create_dir_all(&dir).await?;
        Ok(dir)
    }

    pub async fn remove_track(&self, guild_id: u64, voice_channel_id: u64, track_uid: Uuid) {
        remove(&self.track_path(guild_id, voice_channel_id, track_uid)).await;
    }

    /// Remove a whole voice channel session (and the guild dir if it became empty).
    pub async fn remove_channel(&self, guild_id: u64, voice_channel_id: u64) {
        remove(&self.channel_path(guild_id, voice_channel_id)).await;
        // Fails harmlessly when other channels of the guild still have dirs.
        let _ = tokio::fs::remove_dir(self.root.join(guild_id.to_string())).await;
    }

    /// Wipe everything (startup).
    pub async fn clear(&self) -> std::io::Result<()> {
        remove(&self.root).await;
        tokio::fs::create_dir_all(&self.root).await
    }

    /// Delete the oldest track dirs until the total size fits `STREAM_CACHE_MAX_MB`.
    /// Track dirs for which `protect` returns true are never removed. Returns bytes freed.
    pub async fn enforce_limit<F>(&self, protect: F) -> u64
    where
        F: Fn(u64, u64, &str) -> bool + Send + 'static,
    {
        let root = self.root.clone();
        let max = self.max_bytes;
        tokio::task::spawn_blocking(move || enforce_limit_blocking(&root, max, protect))
            .await
            .unwrap_or(0)
    }
}

async fn remove(path: &Path) {
    if let Err(e) = tokio::fs::remove_dir_all(path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %path.display(), error = %e, "failed to remove stream dir");
        }
    }
}

struct TrackDir {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
    protected: bool,
}

fn enforce_limit_blocking<F: Fn(u64, u64, &str) -> bool>(root: &Path, max: u64, protect: F) -> u64 {
    let mut dirs = Vec::new();
    for (g, gpath) in subdirs(root) {
        for (c, cpath) in subdirs(&gpath) {
            for (t, tpath) in subdirs(&cpath) {
                let (bytes, modified) = dir_usage(&tpath);
                let protected = match (g.parse::<u64>(), c.parse::<u64>()) {
                    (Ok(g), Ok(c)) => protect(g, c, &t),
                    _ => false,
                };
                dirs.push(TrackDir {
                    path: tpath,
                    bytes,
                    modified,
                    protected,
                });
            }
        }
    }
    let mut total: u64 = dirs.iter().map(|d| d.bytes).sum();
    if total <= max {
        return 0;
    }
    dirs.sort_by_key(|d| d.modified);
    let mut freed = 0;
    for d in dirs.iter().filter(|d| !d.protected) {
        if total <= max {
            break;
        }
        if std::fs::remove_dir_all(&d.path).is_ok() {
            total = total.saturating_sub(d.bytes);
            freed += d.bytes;
        }
    }
    freed
}

fn subdirs(p: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(p) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect()
}

/// Total size and newest modification time of everything under `p`.
fn dir_usage(p: &Path) -> (u64, SystemTime) {
    let mut bytes = 0;
    let mut newest = std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut stack = vec![p.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if let Ok(m) = meta.modified() {
                newest = newest.max(m);
            }
            if meta.is_dir() {
                stack.push(e.path());
            } else {
                bytes += meta.len();
            }
        }
    }
    (bytes, newest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        std::env::temp_dir().join(format!("lynx-streams-{}", Uuid::new_v4()))
    }

    #[tokio::test]
    async fn layout_and_removal() {
        let dirs = StreamDirs::new(tmp_root(), 10);
        let t = Uuid::new_v4();
        let owner = ProcOwner::new(11, 22, Some(t));
        let p = dirs.create_for(&owner).await.unwrap();
        assert_eq!(p, dirs.root().join("11").join("22").join(t.to_string()));
        assert!(p.is_dir());
        let other = dirs.create_for(&ProcOwner::new(11, 23, Some(t))).await.unwrap();
        dirs.remove_track(11, 22, t).await;
        assert!(!p.exists());
        dirs.remove_channel(11, 22).await;
        assert!(!dirs.channel_path(11, 22).exists());
        // Guild dir survives while another channel still has files.
        assert!(other.exists());
        dirs.remove_channel(11, 23).await;
        assert!(!dirs.root().join("11").exists());
        dirs.clear().await.unwrap();
        let _ = std::fs::remove_dir_all(dirs.root());
    }

    #[tokio::test]
    async fn enforce_limit_removes_oldest_first_and_respects_protection() {
        let dirs = StreamDirs::new(tmp_root(), 1);
        let ids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
        for (i, id) in ids.iter().enumerate() {
            let p = dirs.create_for(&ProcOwner::new(1, 2, Some(*id))).await.unwrap();
            std::fs::write(p.join("frag"), vec![0u8; 400 * 1024]).unwrap();
            // Distinct mtimes, oldest first.
            let t = SystemTime::now() - std::time::Duration::from_secs(100 - i as u64 * 10);
            let f = std::fs::File::options().write(true).open(p.join("frag")).unwrap();
            f.set_modified(t).unwrap();
            std::fs::File::open(&p).unwrap().set_modified(t).ok();
        }
        // 1.6 MB total, limit 1 MB: two dirs must go. ids[0] is protected, so 1 and 2 go.
        let protected = ids[0].to_string();
        let freed = dirs.enforce_limit(move |_, _, t| t == protected).await;
        assert_eq!(freed, 800 * 1024);
        let exists = |i: usize| dirs.track_path(1, 2, ids[i]).exists();
        assert!(exists(0));
        assert!(!exists(1));
        assert!(!exists(2));
        assert!(exists(3));
        let _ = std::fs::remove_dir_all(dirs.root());
    }
}
