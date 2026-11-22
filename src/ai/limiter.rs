//! Minimal spacing rate limiter shared by every caller of one API (FIFO slot reservation).

use std::time::Duration;

use tokio::{sync::Mutex, time::Instant};
use tokio_util::sync::CancellationToken;

use super::Cancelled;

pub struct RateLimiter {
    interval: Duration,
    next: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: Mutex::new(Instant::now()),
        }
    }

    pub fn per_second(n: u32) -> Self {
        Self::new(Duration::from_millis(1000 / u64::from(n.max(1))))
    }

    /// Wait for the next free slot. A cancelled wait still consumes its slot, which only delays
    /// later callers by one interval at most.
    pub async fn acquire(&self, cancel: &CancellationToken) -> Result<(), Cancelled> {
        let at = {
            let mut next = self.next.lock().await;
            let at = (*next).max(Instant::now());
            *next = at + self.interval;
            at
        };
        tokio::select! {
            _ = tokio::time::sleep_until(at) => Ok(()),
            _ = cancel.cancelled() => Err(Cancelled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spaces_requests() {
        let l = RateLimiter::per_second(50);
        let c = CancellationToken::new();
        let start = Instant::now();
        for _ in 0..6 {
            l.acquire(&c).await.unwrap();
        }
        assert!(start.elapsed() >= Duration::from_millis(100));
        c.cancel();
        assert!(l.acquire(&c).await.is_err());
    }
}
