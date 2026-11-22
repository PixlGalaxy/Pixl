//! "Invite to jam": a voice channel invite whose target is the Lynx Activity (target_type 2), so
//! whoever opens it joins the channel with the panel open. Invites are reused for 12 hours.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde_json::json;
use serenity::{all::ChannelId, http::Http};

use crate::player::messages::discord_error_code;

/// Invite lifetime on Discord (24 h) and how long we reuse one.
const MAX_AGE_SECS: u64 = 24 * 60 * 60;
const REUSE_FOR: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_CACHED: usize = 2048;
const MISSING_PERMISSIONS: isize = 50013;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JamError {
    NoPermission,
    Failed,
}

#[derive(Default)]
pub struct JamInvites {
    codes: DashMap<u64, (String, Instant)>,
}

impl JamInvites {
    pub fn new() -> Self {
        Self::default()
    }

    fn cached(&self, channel: u64) -> Option<String> {
        self.codes
            .get(&channel)
            .filter(|e| e.1.elapsed() < REUSE_FOR)
            .map(|e| e.0.clone())
    }

    fn insert(&self, channel: u64, code: String) {
        if self.codes.len() >= MAX_CACHED {
            self.codes.retain(|_, (_, at)| at.elapsed() < REUSE_FOR);
            if self.codes.len() >= MAX_CACHED {
                self.codes.clear();
            }
        }
        self.codes.insert(channel, (code, Instant::now()));
    }

    /// Invite code for the voice channel (cached or new).
    pub async fn get_or_create(&self, http: &Http, application_id: u64, channel: u64) -> Result<String, JamError> {
        if let Some(code) = self.cached(channel) {
            return Ok(code);
        }
        let body = json!({
            "max_age": MAX_AGE_SECS,
            "max_uses": 0,
            "unique": false,
            "target_type": 2,
            "target_application_id": application_id.to_string(),
        });
        match tokio::time::timeout(
            Duration::from_secs(10),
            http.create_invite(ChannelId::new(channel), &body, None),
        )
        .await
        {
            Ok(Ok(invite)) => {
                self.insert(channel, invite.code.clone());
                Ok(invite.code)
            }
            Ok(Err(e)) if discord_error_code(&e) == Some(MISSING_PERMISSIONS) => Err(JamError::NoPermission),
            Ok(Err(e)) => {
                tracing::warn!(channel, error = %e, "could not create a jam invite");
                Err(JamError::Failed)
            }
            Err(_) => Err(JamError::Failed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_reuse_and_bound() {
        let j = JamInvites::new();
        assert!(j.cached(1).is_none());
        j.insert(1, "abc".into());
        assert_eq!(j.cached(1).as_deref(), Some("abc"));
        for i in 0..(MAX_CACHED as u64 + 10) {
            j.insert(i + 10, "x".into());
        }
        assert!(j.codes.len() <= MAX_CACHED);
    }
}
