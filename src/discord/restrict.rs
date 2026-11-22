//! Channel restrictions set by managers in the panel: where music commands and buttons work.
//! Managers are never restricted. The voice side lives in [`super::play::check_voice`].

use serenity::{all::Interaction, http::Http};

use super::{
    respond,
    ui::{callback, cards},
    Lynx,
};
use crate::perms::channel_allowed;

/// `<#a>, <#b>` for a blocked message.
pub fn mentions(channels: &[u64]) -> String {
    channels
        .iter()
        .map(|c| format!("<#{c}>"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Answer a slash command or button used outside the allowed text channels. Returns true when the
/// interaction was blocked (and answered).
pub async fn reject(lynx: &Lynx, http: &Http, interaction: &Interaction) -> bool {
    let (id, token, guild, channel, user, perms) = match interaction {
        Interaction::Command(c) => (
            c.id,
            c.token.as_str(),
            c.guild_id,
            c.channel_id.get(),
            c.user.id.get(),
            c.member.as_ref().and_then(|m| m.permissions),
        ),
        Interaction::Component(c) => (
            c.id,
            c.token.as_str(),
            c.guild_id,
            c.channel_id.get(),
            c.user.id.get(),
            c.member.as_ref().and_then(|m| m.permissions),
        ),
        _ => return false,
    };
    let Some(guild) = guild.map(|g| g.get()) else {
        return false;
    };
    let rules = lynx.store.channel_rules(guild).await.unwrap_or_default();
    if channel_allowed(&rules.command_channels, channel, false) {
        return false;
    }
    let manager = respond::is_manager(&lynx.bots, guild, user, perms.map(|p| p.bits())).await;
    if channel_allowed(&rules.command_channels, channel, manager) {
        return false;
    }
    let msg = cards::error_key(
        "command_channel",
        &[("channels", &mentions(&rules.command_channels))],
        &lynx.i18n,
    );
    respond::respond(http, id, token, &msg.to_callback(callback::MESSAGE)).await;
    true
}
