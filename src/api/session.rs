//! Panel sessions (random 32-byte tokens, in memory, bounded, with expiry), plus the small bounded
//! caches and fixed-window rate limiters the API uses.

use std::{
    hash::Hash,
    sync::Arc,
    time::{Duration, Instant},
};

use dashmap::DashMap;

/// One signed-in panel user. The guild was verified at sign-in (membership through the bot).
#[derive(Debug, Clone)]
pub struct Session {
    pub user_id: u64,
    pub username: String,
    pub global_name: Option<String>,
    /// Raw Discord CDN URL.
    pub avatar_url: Option<String>,
    pub guild_id: Option<u64>,
    /// Voice channel the Activity was launched in (verified to belong to the guild).
    pub channel_id: Option<u64>,
    pub expires: Instant,
}

impl Session {
    pub fn display_name(&self) -> &str {
        self.global_name.as_deref().unwrap_or(&self.username)
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.expires
    }
}

pub struct Sessions {
    map: DashMap<String, Arc<Session>>,
    ttl: Duration,
    max: usize,
}

impl Sessions {
    pub fn new(ttl: Duration, max: usize) -> Self {
        Self {
            map: DashMap::new(),
            ttl,
            max: max.max(1),
        }
    }

    /// Store a session and return its token.
    pub fn create(&self, mut session: Session) -> String {
        if self.map.len() >= self.max {
            self.sweep();
            if self.map.len() >= self.max {
                // Still full of live sessions: drop the ones closest to expiry.
                let mut all: Vec<(String, Instant)> = self.map.iter().map(|e| (e.key().clone(), e.expires)).collect();
                all.sort_by_key(|(_, at)| *at);
                for (k, _) in all.into_iter().take(self.max / 10 + 1) {
                    self.map.remove(&k);
                }
            }
        }
        session.expires = Instant::now() + self.ttl;
        let token = new_token();
        self.map.insert(token.clone(), Arc::new(session));
        token
    }

    pub fn get(&self, token: &str) -> Option<Arc<Session>> {
        if token.len() != 64 {
            return None;
        }
        let s = self.map.get(token)?.clone();
        if s.expired() {
            self.map.remove(token);
            return None;
        }
        Some(s)
    }

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

/// 32 random bytes from the OS, hex encoded.
fn new_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS random source");
    let mut out = String::with_capacity(64);
    for b in buf {
        out.push_str(&format!("{b:02x}"));
    }
    out
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

    /// Count a hit; false when the key is over its limit.
    pub fn allow(&self, key: K) -> bool {
        if self.map.len() >= self.max_keys && !self.map.contains_key(&key) {
            self.sweep();
            if self.map.len() >= self.max_keys {
                self.map.clear();
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
            channel_id: None,
            expires: Instant::now(),
        }
    }

    #[test]
    fn sessions_expire_and_stay_bounded() {
        let s = Sessions::new(Duration::from_secs(60), 3);
        let t = s.create(session());
        assert_eq!(t.len(), 64);
        assert!(s.get(&t).is_some());
        assert!(s.get("nope").is_none());
        for _ in 0..10 {
            s.create(session());
        }
        assert!(s.len() <= 3);

        let expired = Sessions::new(Duration::ZERO, 10);
        let t = expired.create(session());
        assert!(expired.get(&t).is_none());
        assert_eq!(expired.len(), 0, "an expired session is dropped on access");
    }

    #[test]
    fn limiter_and_cache() {
        let l = RateLimiter::new(2, Duration::from_secs(60), 2);
        assert!(l.allow(1));
        assert!(l.allow(1));
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
