//! One-time cleanup of slash commands left by the previous bot (PIXL) on the same applications.
//!
//! The main bot's global commands are replaced by `commands::register_main` (bulk overwrite), but
//! the old bot also left per-guild commands on the main app, and global commands on the apps now
//! used as voice-only bots (Lynx only manages their guild commands). Runs once per database.

use std::time::Duration;

use serde_json::json;
use serenity::all::GuildId;
use tracing::{info, warn};

use super::{commands, Lynx};

const META_DONE: &str = "legacy_commands_cleanup_v1";
const TIMEOUT: Duration = Duration::from_secs(15);

pub async fn cleanup(lynx: &Lynx) {
    if lynx.store.meta_get(META_DONE).await.ok().flatten().is_some() {
        return;
    }
    info!("removing slash commands left by the previous bot");
    let mut ok = true;

    // Main bot: it only uses global commands, so any guild command is a leftover.
    let main = lynx.bots.main();
    for g in lynx.bots.guilds_of(0) {
        ok &= clear_guild(&main.http, g, "main").await;
    }

    // Voice-only bots: no global commands at all; guild commands only where the main bot is absent.
    for node in lynx.bots.nodes() {
        let app = node.application_id();
        if app == 0 {
            continue;
        }
        match tokio::time::timeout(TIMEOUT, node.http.create_global_commands(&json!([]))).await {
            Ok(Ok(_)) => info!(bot = node.index, "removed old global commands"),
            other => {
                ok = false;
                warn!(bot = node.index, result = ?other.map(|r| r.err().map(|e| e.to_string())), "could not remove old global commands");
            }
        }
        for g in lynx.bots.guilds_of(node.index) {
            // Forget what was stored so `reconcile_nodes` registers the current set where needed.
            let _ = lynx.store.delete_node_guild_commands(app, g).await;
            if lynx.bots.main_in_guild(g) {
                ok &= clear_guild(&node.http, g, "voice").await;
            }
        }
    }

    // Re-send the main bot's global commands on this start, whatever the stored hash says.
    let _ = lynx.store.meta_set(commands::META_COMMANDS_HASH, "").await;
    commands::register_main(lynx).await;

    if ok {
        let _ = lynx.store.meta_set(META_DONE, "1").await;
        info!("old slash commands removed");
    } else {
        warn!("old slash command cleanup incomplete; it will run again on the next start");
    }
}

/// Remove every guild command of this app in `guild_id` (only calls the bulk endpoint if any exist).
async fn clear_guild(http: &serenity::http::Http, guild_id: u64, bot: &str) -> bool {
    let gid = GuildId::new(guild_id);
    match tokio::time::timeout(TIMEOUT, http.get_guild_commands(gid)).await {
        Ok(Ok(list)) if list.is_empty() => true,
        Ok(Ok(list)) => match tokio::time::timeout(TIMEOUT, http.create_guild_commands(gid, &json!([]))).await {
            Ok(Ok(_)) => {
                info!(bot, guild = guild_id, removed = list.len(), "removed old guild commands");
                true
            }
            _ => {
                warn!(bot, guild = guild_id, "could not remove old guild commands");
                false
            }
        },
        // Missing access to a guild's commands is not something a retry fixes.
        Ok(Err(e)) => {
            warn!(bot, guild = guild_id, error = %e, "could not list guild commands");
            true
        }
        Err(_) => false,
    }
}
