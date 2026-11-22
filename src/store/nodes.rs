//! Which node bots have guild commands registered in which guilds (boot reconciliation).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{now_secs, sf, snowflake, Store};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeGuildCommands {
    #[serde(with = "snowflake")]
    pub node_app_id: u64,
    #[serde(with = "snowflake")]
    pub guild_id: u64,
    pub registered_at: i64,
    pub commands_hash: String,
}

type Row = (u64, u64, i64, String);

fn from_row((node_app_id, guild_id, registered_at, commands_hash): Row) -> NodeGuildCommands {
    NodeGuildCommands {
        node_app_id,
        guild_id,
        registered_at,
        commands_hash,
    }
}

impl Store {
    pub async fn node_guild_commands(&self, node_app_id: u64, guild_id: u64) -> Result<Option<NodeGuildCommands>> {
        let row: Option<Row> = sqlx::query_as(
            "SELECT node_app_id, guild_id, registered_at, commands_hash FROM node_guild_commands \
             WHERE node_app_id = ? AND guild_id = ?",
        )
        .bind(sf(node_app_id))
        .bind(sf(guild_id))
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(from_row))
    }

    /// Record that the node registered its commands in the guild (with the hash of the definitions).
    pub async fn set_node_guild_commands(&self, node_app_id: u64, guild_id: u64, commands_hash: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO node_guild_commands (node_app_id, guild_id, registered_at, commands_hash) VALUES (?, ?, ?, ?) \
             ON CONFLICT(node_app_id, guild_id) DO UPDATE SET registered_at = excluded.registered_at, \
             commands_hash = excluded.commands_hash",
        )
        .bind(sf(node_app_id))
        .bind(sf(guild_id))
        .bind(now_secs())
        .bind(commands_hash)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Returns false when nothing was recorded.
    pub async fn delete_node_guild_commands(&self, node_app_id: u64, guild_id: u64) -> Result<bool> {
        let res = sqlx::query("DELETE FROM node_guild_commands WHERE node_app_id = ? AND guild_id = ?")
            .bind(sf(node_app_id))
            .bind(sf(guild_id))
            .execute(self.pool())
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Every guild where this node has commands registered.
    pub async fn list_node_guild_commands(&self, node_app_id: u64) -> Result<Vec<NodeGuildCommands>> {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT node_app_id, guild_id, registered_at, commands_hash FROM node_guild_commands \
             WHERE node_app_id = ? ORDER BY guild_id",
        )
        .bind(sf(node_app_id))
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(from_row).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn crud() {
        let store = Store::open_in_memory().await.unwrap();
        assert!(store.node_guild_commands(10, 1).await.unwrap().is_none());
        store.set_node_guild_commands(10, 1, "h1").await.unwrap();
        store.set_node_guild_commands(10, 2, "h1").await.unwrap();
        store.set_node_guild_commands(11, 1, "h1").await.unwrap();
        store.set_node_guild_commands(10, 1, "h2").await.unwrap();

        assert_eq!(
            store.node_guild_commands(10, 1).await.unwrap().unwrap().commands_hash,
            "h2"
        );
        let list = store.list_node_guild_commands(10).await.unwrap();
        assert_eq!(list.iter().map(|r| r.guild_id).collect::<Vec<_>>(), vec![1, 2]);

        assert!(store.delete_node_guild_commands(10, 1).await.unwrap());
        assert!(!store.delete_node_guild_commands(10, 1).await.unwrap());
        assert_eq!(store.list_node_guild_commands(10).await.unwrap().len(), 1);
        assert!(store.node_guild_commands(11, 1).await.unwrap().is_some());
    }
}
