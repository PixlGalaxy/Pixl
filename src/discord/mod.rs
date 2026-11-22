//! Discord side: the main bot (commands, messages) plus voice-only node bots, all in this process.

pub mod ids;
pub mod ui;

use anyhow::Result;
use serenity::{
    all::{Context, EventHandler, GatewayIntents, Ready},
    async_trait, Client,
};
use songbird::SerenityInit;
use tracing::{error, info};

use crate::AppState;

/// Which bot a client is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotRole {
    Main,
    Node(usize),
}

struct Handler {
    role: BotRole,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(role = ?self.role, user = %ready.user.name, guilds = ready.guilds.len(), "bot ready");
    }
}

/// Start the main bot and every node bot; returns when shutdown is requested.
pub async fn run(state: AppState) -> Result<()> {
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;

    let mut tokens = vec![(BotRole::Main, state.config.discord_token.clone())];
    tokens.extend(state.config.node_tokens.iter().cloned().enumerate().map(|(i, t)| (BotRole::Node(i), t)));

    let mut tasks = Vec::new();
    for (role, token) in tokens {
        let mut client = Client::builder(&token, intents).event_handler(Handler { role }).register_songbird().await?;
        let shard_manager = client.shard_manager.clone();
        let shutdown = state.shutdown.clone();
        tasks.push(tokio::spawn(async move {
            tokio::select! {
                res = client.start_autosharded() => if let Err(e) = res { error!(?role, error = %e, "client stopped") },
                _ = shutdown.cancelled() => shard_manager.shutdown_all().await,
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    Ok(())
}
