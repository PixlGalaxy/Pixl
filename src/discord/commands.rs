//! Slash command definitions (one source for the main bot and the node guild copies), their
//! registration and the node guild-command reconciliation.

use std::time::Duration;

use serde_json::{json, Value};
use serenity::all::{ChannelId, GuildId};
use tracing::{info, warn};

use super::{registry::BotInfo, ui::cards, Lynx};
use crate::i18n::I18n;

pub(crate) const META_COMMANDS_HASH: &str = "commands_hash";
const TIMEOUT: Duration = Duration::from_secs(30);

const STRING: u8 = 3;
const INTEGER: u8 = 4;
const BOOLEAN: u8 = 5;

fn opt(kind: u8, name: &str, description: String, required: bool) -> Value {
    json!({ "type": kind, "name": name, "description": description, "required": required })
}

fn cmd(name: &str, i18n: &I18n, options: Vec<Value>) -> Value {
    json!({
        "type": 1,
        "name": name,
        "description": i18n.t(&format!("bot.cmd.{name}.description"), &[]),
        "options": options,
        "contexts": [0],
        "integration_types": [0],
    })
}

/// Every slash command, in the bot's language. `entry_point` adds the Activity launch command
/// (main bot only); `ai` adds `/ai` (AI_ENABLED).
pub fn definitions(i18n: &I18n, entry_point: bool, ai: bool) -> Value {
    let d = |key: &str| i18n.t(key, &[]);
    let position = |key: &str| {
        let mut o = opt(INTEGER, "position", d(key), true);
        o["min_value"] = json!(1);
        o
    };
    let mut play_query = opt(STRING, "query", d("bot.cmd.play.query"), true);
    play_query["autocomplete"] = json!(true);
    play_query["max_length"] = json!(300);
    let mut from = opt(INTEGER, "from", d("bot.cmd.move.from"), true);
    from["min_value"] = json!(1);
    let mut to = opt(INTEGER, "to", d("bot.cmd.move.to"), true);
    to["min_value"] = json!(1);
    let mut mode = opt(STRING, "mode", d("bot.cmd.loop.mode"), true);
    mode["choices"] = json!([
        { "name": d("bot.cmd.loop.off"), "value": "off" },
        { "name": d("bot.cmd.loop.track"), "value": "track" },
        { "name": d("bot.cmd.loop.queue"), "value": "queue" },
    ]);
    let mut volume = opt(INTEGER, "value", d("bot.cmd.volume.value"), true);
    volume["min_value"] = json!(10);
    volume["max_value"] = json!(100);
    let mut lyrics_query = opt(STRING, "query", d("bot.cmd.lyrics.query"), false);
    lyrics_query["max_length"] = json!(200);

    let mut list = vec![
        cmd("play", i18n, vec![play_query]),
        cmd("pause", i18n, vec![]),
        cmd("resume", i18n, vec![]),
        cmd("skip", i18n, vec![]),
        cmd("stop", i18n, vec![]),
        cmd("queue", i18n, vec![]),
        cmd("nowplaying", i18n, vec![]),
        cmd("remove", i18n, vec![position("bot.cmd.remove.position")]),
        cmd("move", i18n, vec![from, to]),
        cmd("skipto", i18n, vec![position("bot.cmd.skipto.position")]),
        cmd("shuffle", i18n, vec![]),
        cmd("loop", i18n, vec![mode]),
        cmd("volume", i18n, vec![volume]),
        cmd("lyrics", i18n, vec![lyrics_query]),
    ];
    if ai {
        list.push(ai_command(i18n));
    }
    list.push(cmd("panel", i18n, vec![]));
    if entry_point {
        // PRIMARY_ENTRY_POINT with handler DISCORD_LAUNCH_ACTIVITY: Discord launches the Activity itself.
        list.push(json!({
            "type": 4,
            "name": "launch",
            "description": d("bot.cmd.launch.description"),
            "handler": 2,
            "contexts": [0],
            "integration_types": [0],
        }));
    }
    Value::Array(list)
}

/// `/ai`: options for prompt, mix and blend (or none when the modal asks for them).
fn ai_command(i18n: &I18n) -> Value {
    if super::radio::USE_MODAL {
        return cmd("ai", i18n, vec![]);
    }
    let d = |key: &str| i18n.t(key, &[]);
    let mut prompt = opt(STRING, "prompt", d("bot.cmd.ai.prompt"), false);
    prompt["max_length"] = json!(200);
    let mut mix = opt(STRING, "mix", d("bot.cmd.ai.mix"), false);
    mix["choices"] = json!(["favorites", "balanced", "discover"]
        .iter()
        .map(|v| json!({ "name": d(&format!("bot.radio.mix.{v}")), "value": v }))
        .collect::<Vec<_>>());
    let blend = opt(BOOLEAN, "blend", d("bot.cmd.ai.blend"), false);
    cmd("ai", i18n, vec![prompt, mix, blend])
}

/// Stable 64-bit FNV-1a of the JSON (no extra dependency; only used to detect changes).
pub fn hash(defs: &Value) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in defs.to_string().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Bulk overwrite the main bot's global commands, only when the definitions changed.
pub async fn register_main(lynx: &Lynx) {
    let main = lynx.bots.main();
    let defs = definitions(&lynx.i18n, true, lynx.config.ai_enabled);
    let h = hash(&defs);
    if lynx.store.meta_get(META_COMMANDS_HASH).await.ok().flatten().as_deref() == Some(h.as_str()) {
        info!("slash commands unchanged");
        return;
    }
    let first = tokio::time::timeout(TIMEOUT, main.http.create_global_commands(&defs)).await;
    let ok = match first {
        Ok(Ok(_)) => true,
        other => {
            let reason = match other {
                Ok(Err(e)) => e.to_string(),
                _ => "timed out".into(),
            };
            warn!(%reason, "registering commands with the Activity entry point failed; retrying without it");
            let plain = definitions(&lynx.i18n, false, lynx.config.ai_enabled);
            matches!(
                tokio::time::timeout(TIMEOUT, main.http.create_global_commands(&plain)).await,
                Ok(Ok(_))
            )
        }
    };
    if ok {
        info!("slash commands registered");
        if let Err(e) = lynx.store.meta_set(META_COMMANDS_HASH, &h).await {
            warn!(error = %e, "could not store the commands hash");
        }
    } else {
        warn!("slash command registration failed; will retry on the next start");
    }
}

/// Make a node's guild commands match the rule: commands only where the main bot is absent.
/// Calls Discord only when the stored state differs. `announce`: the node just joined for real.
pub async fn sync_node_guild(lynx: &Lynx, node: &BotInfo, guild_id: u64, announce: bool) {
    let app = node.application_id();
    if app == 0 {
        return;
    }
    let defs = definitions(&lynx.i18n, false, lynx.config.ai_enabled);
    let h = hash(&defs);
    let stored = lynx.store.node_guild_commands(app, guild_id).await.ok().flatten();
    let gid = GuildId::new(guild_id);
    if lynx.bots.main_in_guild(guild_id) {
        if stored.is_some() {
            match tokio::time::timeout(TIMEOUT, node.http.create_guild_commands(gid, &json!([]))).await {
                Ok(Ok(_)) => {
                    let _ = lynx.store.delete_node_guild_commands(app, guild_id).await;
                    info!(node = node.index, guild = guild_id, "removed node guild commands");
                }
                _ => warn!(
                    node = node.index,
                    guild = guild_id,
                    "could not remove node guild commands"
                ),
            }
        }
        return;
    }
    if stored.as_ref().map(|s| s.commands_hash.as_str()) != Some(h.as_str()) {
        match tokio::time::timeout(TIMEOUT, node.http.create_guild_commands(gid, &defs)).await {
            Ok(Ok(_)) => {
                let _ = lynx.store.set_node_guild_commands(app, guild_id, &h).await;
                info!(node = node.index, guild = guild_id, "registered node guild commands");
            }
            _ => warn!(
                node = node.index,
                guild = guild_id,
                "could not register node guild commands"
            ),
        }
    }
    if announce {
        announce_add_main(lynx, node, guild_id).await;
    }
}

/// "Add the main bot" in the guild's system channel, if the node may write there.
async fn announce_add_main(lynx: &Lynx, node: &BotInfo, guild_id: u64) {
    let channel = node
        .cache
        .guild(GuildId::new(guild_id))
        .and_then(|g| g.system_channel_id);
    let Some(channel) = channel else { return };
    let allowed = node
        .channel_permissions(guild_id, channel.get())
        .is_some_and(|p| p.administrator() || (p.view_channel() && p.send_messages()));
    if !allowed {
        return;
    }
    let invite = lynx.main_invite();
    let body = cards::node_notice(&invite, &lynx.i18n).to_json();
    let _ = tokio::time::timeout(
        TIMEOUT,
        node.http.send_message(ChannelId::new(channel.get()), vec![], &body),
    )
    .await;
}

/// Startup: compare every node's guilds with `node_guild_commands` and fix only the differences.
pub async fn reconcile_nodes(lynx: &Lynx) {
    for node in lynx.bots.nodes() {
        let app = node.application_id();
        if app == 0 {
            continue;
        }
        let guilds = lynx.bots.guilds_of(node.index);
        for g in &guilds {
            sync_node_guild(lynx, node, *g, false).await;
        }
        // Records of guilds the node left while offline: the commands went with it.
        if let Ok(records) = lynx.store.list_node_guild_commands(app).await {
            for r in records.into_iter().filter(|r| !guilds.contains(&r.guild_id)) {
                let _ = lynx.store.delete_node_guild_commands(app, r.guild_id).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_shape() {
        let i18n = I18n::load("en").unwrap();
        let defs = definitions(&i18n, true, false);
        let list = defs.as_array().unwrap();
        let names: Vec<&str> = list.iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "play",
                "pause",
                "resume",
                "skip",
                "stop",
                "queue",
                "nowplaying",
                "remove",
                "move",
                "skipto",
                "shuffle",
                "loop",
                "volume",
                "lyrics",
                "panel",
                "launch"
            ]
        );
        for c in list {
            let desc = c["description"].as_str().unwrap();
            assert!(
                !desc.is_empty() && desc.len() <= 100 && !desc.starts_with("bot."),
                "{desc}"
            );
            for o in c["options"].as_array().into_iter().flatten() {
                let d = o["description"].as_str().unwrap();
                assert!(!d.is_empty() && d.len() <= 100 && !d.starts_with("bot."), "{d}");
            }
        }
        let launch = list.last().unwrap();
        assert_eq!(
            (launch["type"].as_u64(), launch["handler"].as_u64()),
            (Some(4), Some(2))
        );
        assert_eq!(list[0]["options"][0]["autocomplete"], true);
        assert_eq!(list[12]["options"][0]["max_value"], 100);

        // Nodes get the same commands without the entry point.
        let node = definitions(&i18n, false, false);
        assert_eq!(node.as_array().unwrap().len(), list.len() - 1);
        assert_eq!(node.as_array().unwrap()[..], list[..list.len() - 1]);

        // The hash is stable and changes with the language.
        assert_eq!(hash(&defs), hash(&definitions(&i18n, true, false)));
        assert_ne!(hash(&defs), hash(&definitions(&I18n::load("es").unwrap(), true, false)));
        assert_eq!(hash(&defs).len(), 16);

        // /ai only with AI_ENABLED, before panel; it changes the hash.
        let with_ai = definitions(&i18n, true, true);
        let ai_list = with_ai.as_array().unwrap();
        assert_eq!(ai_list.len(), list.len() + 1);
        let ai = &ai_list[ai_list.len() - 3];
        assert_eq!(ai["name"], "ai");
        assert_eq!(ai_list[ai_list.len() - 2]["name"], "panel");
        assert_ne!(hash(&with_ai), hash(&defs));
        for o in ai["options"].as_array().unwrap() {
            assert_eq!(o["required"], false);
            let d = o["description"].as_str().unwrap();
            assert!(!d.is_empty() && d.len() <= 100 && !d.starts_with("bot."), "{d}");
        }
        assert_eq!(
            ai["options"][1]["choices"][1],
            json!({ "name": "Balanced", "value": "balanced" })
        );
        assert_eq!(ai["options"][2]["type"], 5);
        let desc = ai["description"].as_str().unwrap();
        assert!(!desc.starts_with("bot.") && desc.len() <= 100);
    }
}
