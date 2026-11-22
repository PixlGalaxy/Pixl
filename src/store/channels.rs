//! Per-guild channel restrictions (where music commands work, where Lynx may play) and the music
//! channel (where every automatic message goes). Empty lists / no music channel mean no restriction.

use anyhow::{bail, Result};

use super::{sf, Store};

/// Most channels a list may hold.
pub const MAX_RULE_CHANNELS: usize = 25;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRules {
    /// Text channels (including a voice channel's chat) where commands and buttons work.
    pub command_channels: Vec<u64>,
    /// Voice channels where Lynx may play.
    pub voice_channels: Vec<u64>,
    /// Text channel (or a voice channel's chat) for every automatic message; when set, commands
    /// and buttons work only there and in `command_channels`.
    pub music_channel: Option<u64>,
    /// Managers ignore every rule above (default on).
    pub managers_exempt: bool,
}

impl Default for ChannelRules {
    fn default() -> Self {
        Self {
            command_channels: Vec::new(),
            voice_channels: Vec::new(),
            music_channel: None,
            managers_exempt: true,
        }
    }
}

/// Partial update of [`ChannelOptions`]; `music_channel: Some(None)` clears it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelOptionsPatch {
    pub music_channel: Option<Option<u64>>,
    pub managers_exempt: Option<bool>,
}

impl ChannelRules {
    /// This user ignores every channel rule (a manager, while managers are exempt).
    pub fn exempt(&self, is_manager: bool) -> bool {
        is_manager && self.managers_exempt
    }

    /// Voice restriction for one user.
    pub fn voice_allowed_for(&self, channel: u64, is_manager: bool) -> bool {
        self.exempt(is_manager) || self.voice_allowed(channel)
    }

    pub fn command_allowed(&self, channel: u64) -> bool {
        self.command_channels.is_empty() || self.command_channels.contains(&channel)
    }

    pub fn voice_allowed(&self, channel: u64) -> bool {
        self.voice_channels.is_empty() || self.voice_channels.contains(&channel)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    Command,
    Voice,
}

impl RuleKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Voice => "voice",
        }
    }
}

impl Store {
    pub async fn channel_rules(&self, guild_id: u64) -> Result<ChannelRules> {
        let rows: Vec<(String, u64)> =
            sqlx::query_as("SELECT kind, channel_id FROM guild_channel_rules WHERE guild_id = ? ORDER BY channel_id")
                .bind(sf(guild_id))
                .fetch_all(self.pool())
                .await?;
        let options: Option<(Option<u64>, bool)> =
            sqlx::query_as("SELECT music_channel_id, managers_exempt FROM guild_channel_options WHERE guild_id = ?")
                .bind(sf(guild_id))
                .fetch_optional(self.pool())
                .await?;
        let mut out = ChannelRules::default();
        if let Some((music, exempt)) = options {
            out.music_channel = music.filter(|c| *c > 0);
            out.managers_exempt = exempt;
        }
        for (kind, id) in rows {
            if kind == "voice" {
                out.voice_channels.push(id);
            } else {
                out.command_channels.push(id);
            }
        }
        Ok(out)
    }

    pub async fn music_channel(&self, guild_id: u64) -> Result<Option<u64>> {
        let row: Option<(Option<u64>,)> =
            sqlx::query_as("SELECT music_channel_id FROM guild_channel_options WHERE guild_id = ?")
                .bind(sf(guild_id))
                .fetch_optional(self.pool())
                .await?;
        Ok(row.and_then(|r| r.0).filter(|c| *c > 0))
    }

    /// Apply a partial update of the music channel / manager exemption.
    pub async fn update_channel_options(&self, guild_id: u64, patch: ChannelOptionsPatch) -> Result<()> {
        if patch == ChannelOptionsPatch::default() {
            return Ok(());
        }
        let mut tx = self.begin_write().await?;
        let row: Option<(Option<u64>, bool)> =
            sqlx::query_as("SELECT music_channel_id, managers_exempt FROM guild_channel_options WHERE guild_id = ?")
                .bind(sf(guild_id))
                .fetch_optional(&mut *tx)
                .await?;
        let (mut music, mut exempt) = row.unwrap_or((None, true));
        if let Some(m) = patch.music_channel {
            music = m.filter(|c| *c > 0);
        }
        if let Some(e) = patch.managers_exempt {
            exempt = e;
        }
        sqlx::query(
            "INSERT INTO guild_channel_options (guild_id, music_channel_id, managers_exempt) VALUES (?, ?, ?)              ON CONFLICT(guild_id) DO UPDATE SET music_channel_id = excluded.music_channel_id,              managers_exempt = excluded.managers_exempt",
        )
        .bind(sf(guild_id))
        .bind(music.map(sf))
        .bind(exempt)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Replace one list (deduplicated). Fails above [`MAX_RULE_CHANNELS`].
    pub async fn set_channel_rules(&self, guild_id: u64, kind: RuleKind, channels: &[u64]) -> Result<()> {
        let mut list: Vec<u64> = channels.iter().copied().filter(|c| *c > 0).collect();
        list.sort_unstable();
        list.dedup();
        if list.len() > MAX_RULE_CHANNELS {
            bail!("at most {MAX_RULE_CHANNELS} channels");
        }
        let mut tx = self.begin_write().await?;
        sqlx::query("DELETE FROM guild_channel_rules WHERE guild_id = ? AND kind = ?")
            .bind(sf(guild_id))
            .bind(kind.as_str())
            .execute(&mut *tx)
            .await?;
        for c in list {
            sqlx::query("INSERT INTO guild_channel_rules (guild_id, kind, channel_id) VALUES (?, ?, ?)")
                .bind(sf(guild_id))
                .bind(kind.as_str())
                .bind(sf(c))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rules_roundtrip() {
        let store = Store::open_in_memory().await.unwrap();
        let r = store.channel_rules(1).await.unwrap();
        assert_eq!(r, ChannelRules::default());
        assert!(r.command_allowed(5) && r.voice_allowed(5), "empty = no restriction");

        store
            .set_channel_rules(1, RuleKind::Voice, &[30, 20, 30])
            .await
            .unwrap();
        store.set_channel_rules(1, RuleKind::Command, &[10]).await.unwrap();
        let r = store.channel_rules(1).await.unwrap();
        assert_eq!(r.voice_channels, vec![20, 30]);
        assert_eq!(r.command_channels, vec![10]);
        assert!(r.voice_allowed(20) && !r.voice_allowed(21));
        assert!(store.channel_rules(2).await.unwrap().voice_channels.is_empty());

        store.set_channel_rules(1, RuleKind::Voice, &[]).await.unwrap();
        assert!(store.channel_rules(1).await.unwrap().voice_channels.is_empty());
        let many: Vec<u64> = (1..=26).collect();
        assert!(store.set_channel_rules(1, RuleKind::Voice, &many).await.is_err());
    }

    #[tokio::test]
    async fn options_roundtrip() {
        let store = Store::open_in_memory().await.unwrap();
        let r = store.channel_rules(1).await.unwrap();
        assert_eq!((r.music_channel, r.managers_exempt), (None, true), "defaults");
        assert!(r.exempt(true) && !r.exempt(false));

        let set = |m: Option<Option<u64>>, e: Option<bool>| ChannelOptionsPatch {
            music_channel: m,
            managers_exempt: e,
        };
        store
            .update_channel_options(1, set(Some(Some(40)), None))
            .await
            .unwrap();
        store
            .update_channel_options(1, set(Some(Some(41)), None))
            .await
            .unwrap();
        assert_eq!(store.music_channel(1).await.unwrap(), Some(41));
        store.update_channel_options(1, set(None, Some(false))).await.unwrap();
        let r = store.channel_rules(1).await.unwrap();
        assert_eq!(
            (r.music_channel, r.managers_exempt),
            (Some(41), false),
            "the other field is kept"
        );
        assert!(!r.exempt(true));
        assert_eq!(store.music_channel(2).await.unwrap(), None);

        store.update_channel_options(1, set(Some(None), None)).await.unwrap();
        let r = store.channel_rules(1).await.unwrap();
        assert_eq!((r.music_channel, r.managers_exempt), (None, false));
    }

    #[test]
    fn voice_rule_follows_the_exemption() {
        let mut r = ChannelRules {
            voice_channels: vec![20],
            ..ChannelRules::default()
        };
        assert!(r.voice_allowed_for(21, true) && !r.voice_allowed_for(21, false));
        r.managers_exempt = false;
        assert!(!r.voice_allowed_for(21, true) && r.voice_allowed_for(20, true));
    }
}
