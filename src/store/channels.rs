//! Per-guild channel restrictions (where music commands work, where Lynx may play).
//! Empty lists mean no restriction.

use anyhow::{bail, Result};

use super::{sf, Store};

/// Most channels a list may hold.
pub const MAX_RULE_CHANNELS: usize = 25;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelRules {
    /// Text channels (including a voice channel's chat) where commands and buttons work.
    pub command_channels: Vec<u64>,
    /// Voice channels where Lynx may play.
    pub voice_channels: Vec<u64>,
}

impl ChannelRules {
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
        let mut out = ChannelRules::default();
        for (kind, id) in rows {
            if kind == "voice" {
                out.voice_channels.push(id);
            } else {
                out.command_channels.push(id);
            }
        }
        Ok(out)
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
}
