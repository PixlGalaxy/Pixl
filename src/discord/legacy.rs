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
        ok &= clear_global(&node.http, node.index).await;
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

/// Delete the app's global commands one by one. The Activity Entry Point command (type 4) can't
/// be removed in a bulk update and is harmless (it only launches the app), so it stays.
async fn clear_global(http: &serenity::http::Http, bot: usize) -> bool {
    let list = match tokio::time::timeout(TIMEOUT, http.get_global_commands()).await {
        Ok(Ok(list)) => list,
        other => {
            warn!(bot, result = ?other.map(|r| r.err().map(|e| e.to_string())), "could not list old global commands");
            return false;
        }
    };
    let mut ok = true;
    let mut removed = 0;
    for c in list.iter().filter(|c| u8::from(c.kind) != 4) {
        match tokio::time::timeout(TIMEOUT, http.delete_global_command(c.id)).await {
            Ok(Ok(())) => removed += 1,
            _ => {
                ok = false;
                warn!(bot, command = %c.name, "could not remove an old global command");
            }
        }
    }
    if removed > 0 {
        info!(bot, removed, "removed old global commands");
    }
    ok
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
