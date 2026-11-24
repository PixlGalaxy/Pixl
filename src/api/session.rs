//! Panel sessions (random 32-byte tokens with expiry), plus the small bounded caches and fixed-window
//! rate limiters the API uses.
//!
//! Sessions live in SQLite so a restart or an update keeps everyone signed in. Only the SHA-256 of a
//! token is stored; a bounded in-memory map (keyed by the same hash) fronts the table.

use std::{
    hash::Hash,
    sync::Arc,
    time::{Duration, Instant},
};

use dashmap::DashMap;
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::store::{now_secs, PanelSessionRow, Store};

/// One signed-in panel user. The guild was verified at sign-in (membership through the bot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub user_id: u64,
    pub username: String,
    pub global_name: Option<String>,
    /// Raw Discord CDN URL.
    pub avatar_url: Option<String>,
    pub guild_id: Option<u64>,
    /// Channel the Activity was opened from (any kind, verified to belong to the guild). Informational
    /// only: the panel always follows the user's current voice channel.
    pub launch_channel_id: Option<u64>,
    pub created_at: i64,
    /// Epoch seconds.
    pub expires_at: i64,
}

impl Session {
    pub fn display_name(&self) -> &str {
        self.global_name.as_deref().unwrap_or(&self.username)
    }

    pub fn expired(&self) -> bool {
        now_secs() >= self.expires_at
    }

    fn to_row(&self, token_hash: String) -> PanelSessionRow {
        PanelSessionRow {
            token_hash,
            user_id: self.user_id,
            username: self.username.clone(),
            global_name: self.global_name.clone(),
            avatar_url: self.avatar_url.clone(),
            guild_id: self.guild_id,
            launch_channel_id: self.launch_channel_id,
            created_at: self.created_at,
            expires_at: self.expires_at,
        }
    }

    fn from_row(r: PanelSessionRow) -> Self {
        Self {
            user_id: r.user_id,
            username: r.username,
            global_name: r.global_name,
            avatar_url: r.avatar_url,
            guild_id: r.guild_id,
            launch_channel_id: r.launch_channel_id,
            created_at: r.created_at,
            expires_at: r.expires_at,
        }
    }
}

pub struct Sessions {
    /// Token hash -> session.
    map: DashMap<String, Arc<Session>>,
    store: Store,
    ttl: Duration,
    max: usize,
}

impl Sessions {
    pub fn new(store: Store, ttl: Duration, max: usize) -> Self {
        Self {
            map: DashMap::new(),
            store,
            ttl,
            max: max.max(1),
        }
    }

    /// Store a session (memory and SQLite) and return its token.
    pub async fn create(&self, mut session: Session) -> String {
        let now = now_secs();
        session.created_at = now;
        session.expires_at = now + self.ttl.as_secs() as i64;
        let token = new_token();
        let hash = hash_token(&token);
        if let Err(e) = self.store.put_panel_session(&session.to_row(hash.clone())).await {
            warn!(error = %e, "could not persist a panel session; it lasts until the next restart");
        }
        self.remember(hash, Arc::new(session));
        token
    }

    /// The live session of a token: memory first, then SQLite (after a restart or an eviction).
    pub async fn get(&self, token: &str) -> Option<Arc<Session>> {
        if token.len() != 64 {
            return None;
        }
        let hash = hash_token(token);
        let cached = self.map.get(&hash).map(|s| s.clone());
        if let Some(s) = cached {
            if s.expired() {
                self.map.remove(&hash);
                let _ = self.store.delete_panel_session(&hash).await;
                return None;
            }
            return Some(s);
        }
        let row = match self.store.panel_session(&hash).await {
            Ok(r) => r?,
            Err(e) => {
                warn!(error = %e, "could not read a panel session");
                return None;
            }
        };
        let s = Arc::new(Session::from_row(row));
        if s.expired() {
            return None;
        }
        self.remember(hash, s.clone());
        Some(s)
    }

    fn remember(&self, hash: String, session: Arc<Session>) {
        if self.map.len() >= self.max {
            self.sweep();
            if self.map.len() >= self.max {
                // Still full of live sessions: forget the ones closest to expiry (they stay in SQLite).
                let mut all: Vec<(String, i64)> = self.map.iter().map(|e| (e.key().clone(), e.expires_at)).collect();
                all.sort_by_key(|(_, at)| *at);
                for (k, _) in all.into_iter().take(self.max / 10 + 1) {
                    self.map.remove(&k);
                }
            }
        }
        self.map.insert(hash, session);
    }

    /// Sign one token out (memory and SQLite). Err when the row could not be deleted.
    pub async fn revoke(&self, token: &str) -> anyhow::Result<()> {
        let hash = hash_token(token);
        self.map.remove(&hash);
        self.store.delete_panel_session(&hash).await
    }

    /// Sign a user out everywhere (memory and SQLite); returns how many stored sessions were removed.
    pub async fn revoke_user(&self, user_id: u64) -> anyhow::Result<u64> {
        self.map.retain(|_, s| s.user_id != user_id);
        let n = self.store.delete_user_panel_sessions(user_id).await?;
        // A lookup that read its row before the delete may have cached it again meanwhile.
        self.map.retain(|_, s| s.user_id != user_id);
        Ok(n)
    }

    /// Drop expired sessions from memory (SQLite rows go in the store maintenance sweep).
    pub fn sweep(&self) -> usize {
        let before = self.map.len();
        self.map.retain(|_, s| !s.expired());
        before - self.map.len()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// SHA-256 of a token, hex encoded: the only form a token is stored in.
pub fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 32 random bytes from the OS, hex encoded.
fn new_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS random source");
    hex(&buf)
}

/// Bounded map whose entries expire. When full it drops expired entries, then everything.
pub struct TtlCache<K, V> {
    map: DashMap<K, (Instant, V)>,
    ttl: Duration,
    max: usize,
}

impl<K: Eq + Hash + Clone, V: Clone> TtlCache<K, V> {
    pub fn new(ttl: Duration, max: usize) -> Self {
        Self {
            map: DashMap::new(),
            ttl,
            max: max.max(1),
        }
    }

    pub fn get(&self, key: &K) -> Option<V> {
        let e = self.map.get(key)?;
        (e.0.elapsed() < self.ttl).then(|| e.1.clone())
    }

    pub fn insert(&self, key: K, value: V) {
        if self.map.len() >= self.max && !self.map.contains_key(&key) {
            let ttl = self.ttl;
            self.map.retain(|_, (at, _)| at.elapsed() < ttl);
            if self.map.len() >= self.max {
                self.map.clear();
            }
        }
        self.map.insert(key, (Instant::now(), value));
    }

    pub fn remove(&self, key: &K) {
        self.map.remove(key);
    }

    pub fn sweep(&self) {
        let ttl = self.ttl;
        self.map.retain(|_, (at, _)| at.elapsed() < ttl);
    }
}

/// Fixed-window limiter: at most `limit` hits per `window` per key. Bounded like [`TtlCache`].
pub struct RateLimiter<K> {
    map: DashMap<K, (Instant, u32)>,
    limit: u32,
    window: Duration,
    max_keys: usize,
}

impl<K: Eq + Hash + Clone> RateLimiter<K> {
    pub fn new(limit: u32, window: Duration, max_keys: usize) -> Self {
        Self {
            map: DashMap::new(),
            limit,
            window,
            max_keys: max_keys.max(1),
        }
    }

    /// Count a hit; false when the key is over its limit. When the map is full of keys still in
    /// their window, a new key is refused (until windows expire) rather than wiping every count:
    /// clearing would let a flood of new keys reset the limits of the keys already over them.
    pub fn allow(&self, key: K) -> bool {
        if self.map.len() >= self.max_keys && !self.map.contains_key(&key) {
            self.sweep();
            if self.map.len() >= self.max_keys {
                return false;
            }
        }
        let mut e = self.map.entry(key).or_insert((Instant::now(), 0));
        if e.0.elapsed() >= self.window {
            *e = (Instant::now(), 0);
        }
        e.1 += 1;
        e.1 <= self.limit
    }

    pub fn sweep(&self) {
        let w = self.window;
        self.map.retain(|_, (at, _)| at.elapsed() < w);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session {
            user_id: 1,
            username: "a".into(),
            global_name: None,
            avatar_url: None,
            guild_id: Some(2),
            launch_channel_id: None,
            created_at: 0,
            expires_at: 0,
        }
    }

    #[tokio::test]
    async fn revoked_sessions_are_gone_from_memory_and_sqlite() {
        let store = Store::open_in_memory().await.unwrap();
        let s = Sessions::new(store.clone(), Duration::from_secs(60), 10);
        let a = s.create(session()).await;
        let b = s.create(session()).await;
        let other = s.create(Session { user_id: 9, ..session() }).await;
        s.revoke(&a).await.unwrap();
        assert!(s.get(&a).await.is_none());
        assert!(s.get(&b).await.is_some());
        assert_eq!(s.revoke_user(1).await.unwrap(), 1);
        assert!(s.get(&b).await.is_none());
        // Not only from the memory cache: a restart (fresh cache) refuses them too.
        let fresh = Sessions::new(store, Duration::from_secs(60), 10);
        assert!(fresh.get(&b).await.is_none());
        assert!(fresh.get(&other).await.is_some());
    }

    #[tokio::test]
    async fn sessions_expire_and_stay_bounded() {
        let store = Store::open_in_memory().await.unwrap();
        let s = Sessions::new(store.clone(), Duration::from_secs(60), 3);
        let t = s.create(session()).await;
        assert_eq!(t.len(), 64);
        assert!(s.get(&t).await.is_some());
        assert!(s.get("nope").await.is_none());
        for _ in 0..10 {
            s.create(session()).await;
        }
        assert!(s.len() <= 3);
        assert!(s.get(&t).await.is_some(), "evicted from memory, still in SQLite");

        let expired = Sessions::new(store, Duration::ZERO, 10);
        let t = expired.create(session()).await;
        assert!(expired.get(&t).await.is_none());
        assert_eq!(expired.len(), 0, "an expired session is dropped on access");
    }

    #[test]
    fn token_hash_is_sha256_hex() {
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn limiter_and_cache() {
        let l = RateLimiter::new(2, Duration::from_secs(60), 2);
        assert!(l.allow(1));
        assert!(l.allow(1));
        assert!(!l.allow(1));
        assert!(l.allow(2));
        // Full of live keys: a new key is refused and the counts already there survive.
        assert!(!l.allow(3));
        assert!(!l.allow(1));
        assert!(l.allow(2));
        let c = TtlCache::new(Duration::from_secs(60), 2);
        c.insert(1, "a");
        c.insert(2, "b");
        c.insert(3, "c");
        assert_eq!(c.get(&3), Some("c"));
        assert!(c.map.len() <= 2);
    }
}
