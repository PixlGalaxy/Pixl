//! Per-user rate limits shared by the panel API and the Discord commands, so both spend the same
//! budget (a user can't double it by switching between them).

use std::{sync::Arc, time::Duration};

use crate::api::session::RateLimiter;

const WINDOW: Duration = Duration::from_secs(60);
const MAX_KEYS: usize = 20_000;

pub struct UserLimits {
    /// Searches: panel search, `/play` and `/lyrics` with a query, `/play` autocomplete.
    pub search: Arc<RateLimiter<u64>>,
    /// Songs added: panel adds, `/play`.
    pub add: Arc<RateLimiter<u64>>,
    /// AI radio starts (Ollama has a single slot for everyone).
    pub ai: Arc<RateLimiter<u64>>,
}

impl Default for UserLimits {
    fn default() -> Self {
        Self {
            search: Arc::new(RateLimiter::new(30, WINDOW, MAX_KEYS)),
            add: Arc::new(RateLimiter::new(15, WINDOW, MAX_KEYS)),
            ai: Arc::new(RateLimiter::new(5, WINDOW, MAX_KEYS)),
        }
    }
}

impl UserLimits {
    pub fn sweep(&self) {
        self.search.sweep();
        self.add.sweep();
        self.ai.sweep();
    }
}
