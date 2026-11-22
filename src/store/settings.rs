//! Per-guild settings (control mode, vote skip, limits).

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use super::{now_secs, sf, Store};

pub const DEFAULT_MAX_QUEUE_LENGTH: u32 = 500;
pub const DEFAULT_VOLUME: u16 = 100;
/// 100 = the original audio (Opus passthrough, no re-encoding). Lynx never amplifies above it.
pub const MAX_VOLUME: u16 = 100;
pub const MAX_QUEUE_LENGTH_LIMIT: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    #[default]
    Everyone,
    Dj,
}

impl ControlMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Everyone => "everyone",
            Self::Dj => "dj",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "dj" {
            Self::Dj
        } else {
            Self::Everyone
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VoteSkipMode {
    Count,
    #[default]
    Percent,
}

impl VoteSkipMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Percent => "percent",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "count" {
            Self::Count
        } else {
            Self::Percent
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoteSkip {
    pub enabled: bool,
    pub mode: VoteSkipMode,
    /// Votes needed (count) or percentage of listeners (percent, 1-100).
    pub value: u32,
}

impl Default for VoteSkip {
    fn default() -> Self {
        Self { enabled: false, mode: VoteSkipMode::Percent, value: 50 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuildSettings {
    pub control_mode: ControlMode,
    pub vote_skip: VoteSkip,
    pub max_queue_length: u32,
    pub default_volume: u16,
}

impl Default for GuildSettings {
    fn default() -> Self {
        Self {
            control_mode: ControlMode::Everyone,
            vote_skip: VoteSkip::default(),
            max_queue_length: DEFAULT_MAX_QUEUE_LENGTH,
            default_volume: DEFAULT_VOLUME,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoteSkipPatch {
    pub enabled: Option<bool>,
    pub mode: Option<VoteSkipMode>,
    pub value: Option<u32>,
}

/// Body of `PUT /api/settings`: every field optional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuildSettingsPatch {
    pub control_mode: Option<ControlMode>,
    pub vote_skip: Option<VoteSkipPatch>,
    pub max_queue_length: Option<u32>,
    pub default_volume: Option<u16>,
}

impl GuildSettings {
    pub fn apply(&mut self, patch: &GuildSettingsPatch) {
        if let Some(m) = patch.control_mode {
            self.control_mode = m;
        }
        if let Some(v) = patch.vote_skip {
            if let Some(e) = v.enabled {
                self.vote_skip.enabled = e;
            }
            if let Some(m) = v.mode {
                self.vote_skip.mode = m;
            }
            if let Some(x) = v.value {
                self.vote_skip.value = x;
            }
        }
        if let Some(n) = patch.max_queue_length {
            self.max_queue_length = n;
        }
        if let Some(v) = patch.default_volume {
            self.default_volume = v;
        }
    }

    /// Returns a short reason when a value is out of range.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.default_volume > MAX_VOLUME {
            return Err("defaultVolume must be 0-100");
        }
        if self.max_queue_length == 0 || self.max_queue_length > MAX_QUEUE_LENGTH_LIMIT {
            return Err("maxQueueLength must be 1-10000");
        }
        match self.vote_skip.mode {
            VoteSkipMode::Percent if !(1..=100).contains(&self.vote_skip.value) => {
                Err("voteSkip.value must be 1-100 in percent mode")
            }
            VoteSkipMode::Count if !(1..=1000).contains(&self.vote_skip.value) => {
                Err("voteSkip.value must be 1-1000 in count mode")
            }
            _ => Ok(()),
        }
    }
}

type SettingsRow = (String, bool, String, u32, u32, u16);

fn from_row(r: SettingsRow) -> GuildSettings {
    GuildSettings {
        control_mode: ControlMode::parse(&r.0),
        vote_skip: VoteSkip { enabled: r.1, mode: VoteSkipMode::parse(&r.2), value: r.3 },
        max_queue_length: r.4,
        default_volume: r.5,
    }
}

impl Store {
    /// Settings of a guild, or the defaults when it never changed anything.
    pub async fn guild_settings(&self, guild_id: u64) -> Result<GuildSettings> {
        let row: Option<SettingsRow> = sqlx::query_as(
            "SELECT control_mode, vote_skip_enabled, vote_skip_mode, vote_skip_value, max_queue_length, default_volume \
             FROM guild_settings WHERE guild_id = ?",
        )
        .bind(sf(guild_id))
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(from_row).unwrap_or_default())
    }

    /// Apply a partial update and return the resulting settings. Fails if the result is invalid.
    pub async fn update_guild_settings(&self, guild_id: u64, patch: &GuildSettingsPatch) -> Result<GuildSettings> {
        let mut tx = self.begin_write().await?;
        let row: Option<SettingsRow> = sqlx::query_as(
            "SELECT control_mode, vote_skip_enabled, vote_skip_mode, vote_skip_value, max_queue_length, default_volume \
             FROM guild_settings WHERE guild_id = ?",
        )
        .bind(sf(guild_id))
        .fetch_optional(&mut *tx)
        .await?;
        let mut s = row.map(from_row).unwrap_or_default();
        s.apply(patch);
        if let Err(reason) = s.validate() {
            bail!("invalid settings: {reason}");
        }
        sqlx::query(
            "INSERT INTO guild_settings (guild_id, control_mode, vote_skip_enabled, vote_skip_mode, vote_skip_value, \
             max_queue_length, default_volume, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(guild_id) DO UPDATE SET control_mode = excluded.control_mode, \
             vote_skip_enabled = excluded.vote_skip_enabled, vote_skip_mode = excluded.vote_skip_mode, \
             vote_skip_value = excluded.vote_skip_value, max_queue_length = excluded.max_queue_length, \
             default_volume = excluded.default_volume, updated_at = excluded.updated_at",
        )
        .bind(sf(guild_id))
        .bind(s.control_mode.as_str())
        .bind(s.vote_skip.enabled)
        .bind(s.vote_skip.mode.as_str())
        .bind(s.vote_skip.value)
        .bind(s.max_queue_length)
        .bind(s.default_volume)
        .bind(now_secs())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(s)
    }

    /// Forget a guild's settings (back to defaults).
    pub async fn reset_guild_settings(&self, guild_id: u64) -> Result<()> {
        sqlx::query("DELETE FROM guild_settings WHERE guild_id = ?").bind(sf(guild_id)).execute(self.pool()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn defaults_when_absent() {
        let store = Store::open_in_memory().await.unwrap();
        let s = store.guild_settings(1).await.unwrap();
        assert_eq!(s.control_mode, ControlMode::Everyone);
        assert!(!s.vote_skip.enabled);
        assert_eq!(s.max_queue_length, 500);
        assert_eq!(s.default_volume, 100);
    }

    #[tokio::test]
    async fn partial_update_keeps_other_fields() {
        let store = Store::open_in_memory().await.unwrap();
        let patch = GuildSettingsPatch { control_mode: Some(ControlMode::Dj), ..Default::default() };
        store.update_guild_settings(7, &patch).await.unwrap();
        let patch = GuildSettingsPatch {
            vote_skip: Some(VoteSkipPatch { enabled: Some(true), mode: Some(VoteSkipMode::Count), value: Some(3) }),
            ..Default::default()
        };
        let s = store.update_guild_settings(7, &patch).await.unwrap();
        assert_eq!(s, store.guild_settings(7).await.unwrap());
        assert_eq!(s.control_mode, ControlMode::Dj);
        assert_eq!(s.vote_skip, VoteSkip { enabled: true, mode: VoteSkipMode::Count, value: 3 });
        assert_eq!(s.max_queue_length, 500);
        // Other guilds untouched.
        assert_eq!(store.guild_settings(8).await.unwrap(), GuildSettings::default());
    }

    #[tokio::test]
    async fn rejects_invalid() {
        let store = Store::open_in_memory().await.unwrap();
        let patch = GuildSettingsPatch { default_volume: Some(500), ..Default::default() };
        assert!(store.update_guild_settings(1, &patch).await.is_err());
        assert_eq!(store.guild_settings(1).await.unwrap(), GuildSettings::default());
    }

    #[test]
    fn patch_json_shape() {
        let p: GuildSettingsPatch =
            serde_json::from_str(r#"{"controlMode":"dj","voteSkip":{"enabled":true},"defaultVolume":80}"#).unwrap();
        assert_eq!(p.control_mode, Some(ControlMode::Dj));
        assert_eq!(p.vote_skip.unwrap().enabled, Some(true));
        let json = serde_json::to_value(GuildSettings::default()).unwrap();
        assert_eq!(json["voteSkip"]["mode"], "percent");
        assert_eq!(json["maxQueueLength"], 500);
    }
}
