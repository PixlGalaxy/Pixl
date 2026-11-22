//! Raw interaction responses and the guild-member helpers shared by commands and the panel API.
//! Every network call has a timeout; failures are logged, never panicking.

use std::time::Duration;

use serde_json::Value;
use serenity::{
    all::{GuildId, InteractionId, UserId},
    http::Http,
};
use tracing::debug;

use super::registry::BotRegistry;
use crate::{
    perms::{is_manager_bits, Actor},
    store::Store,
};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Initial interaction callback (must happen within 3 s of the interaction).
pub async fn respond(http: &Http, id: InteractionId, token: &str, body: &Value) -> bool {
    match tokio::time::timeout(TIMEOUT, http.create_interaction_response(id, token, body, vec![])).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            debug!(error = %e, "interaction response failed");
            false
        }
        Err(_) => {
            debug!("interaction response timed out");
            false
        }
    }
}

/// Edit the deferred / original response.
pub async fn edit_original(http: &Http, token: &str, body: &Value) -> bool {
    matches!(
        tokio::time::timeout(TIMEOUT, http.edit_original_interaction_response(token, body, vec![])).await,
        Ok(Ok(_))
    )
}

pub async fn delete_original(http: &Http, token: &str) {
    let _ = tokio::time::timeout(TIMEOUT, http.delete_original_interaction_response(token)).await;
}

pub async fn followup(http: &Http, token: &str, body: &Value) -> bool {
    matches!(
        tokio::time::timeout(TIMEOUT, http.create_followup_message(token, body, vec![])).await,
        Ok(Ok(_))
    )
}

/// Lynx manager: owner, Administrator or Manage Server. `interaction_permissions` are the member's
/// resolved permissions sent with an interaction; without them (panel) they are computed from the
/// member (REST) and the guild roles (cache).
pub async fn is_manager(bots: &BotRegistry, guild_id: u64, user_id: u64, interaction_permissions: Option<u64>) -> bool {
    let owner = bots.guild_owner(guild_id) == Some(user_id);
    if let Some(bits) = interaction_permissions {
        return is_manager_bits(bits, owner);
    }
    if owner {
        return true;
    }
    let main = bots.main();
    let member = match tokio::time::timeout(
        TIMEOUT,
        main.http.get_member(GuildId::new(guild_id), UserId::new(user_id)),
    )
    .await
    {
        Ok(Ok(m)) => m,
        _ => return false,
    };
    let Some(guild) = main.cache.guild(GuildId::new(guild_id)) else {
        return false;
    };
    let bits = guild.member_permissions(&member).bits();
    is_manager_bits(bits, false)
}

/// Build the permission actor of a user for the player in `player_channel`.
pub async fn actor(
    bots: &BotRegistry,
    store: &Store,
    guild_id: u64,
    user_id: u64,
    interaction_permissions: Option<u64>,
    player_channel: u64,
    requested_current: bool,
) -> Actor {
    let is_manager = is_manager(bots, guild_id, user_id, interaction_permissions).await;
    let is_dj = store.is_dj(guild_id, user_id).await.unwrap_or(false);
    let in_channel = bots.user_voice_channel(guild_id, user_id) == Some(player_channel);
    Actor {
        is_manager,
        is_dj,
        in_channel,
        requested_current,
    }
}
