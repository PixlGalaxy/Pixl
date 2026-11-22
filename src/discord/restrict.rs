//! Channel restrictions set by managers in the panel: where music commands and buttons work.
//! Managers are not restricted unless the guild turned that off. The voice side lives in [`super::play::check_voice`].

use serenity::{all::Interaction, http::Http};

use super::{
    respond,
    ui::{callback, cards},
    Lynx,
};
use crate::{perms::channel_allowed, store::ChannelRules};

/// `<#a>, <#b>` for a blocked message.
pub fn mentions(channels: &[u64]) -> String {
    channels
        .iter()
        .map(|c| format!("<#{c}>"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Commands that work in every channel, whatever the restrictions.
const ANYWHERE: [&str; 1] = ["invite"];

/// Where an interaction may run. `None` = allowed; `Some(list)` = blocked, use one of `list`.
///
/// - `/invite` works everywhere; managers are not restricted while `managers_exempt` is on.
/// - With a music channel: only there and in the command channels.
/// - Otherwise: the command channels (an empty list allows every channel).
pub fn blocked(rules: &ChannelRules, channel: u64, command: Option<&str>, manager: bool) -> Option<Vec<u64>> {
    if rules.exempt(manager) || command.is_some_and(|c| ANYWHERE.contains(&c)) {
        return None;
    }
    match rules.music_channel {
        Some(music) => {
            if channel == music || rules.command_channels.contains(&channel) {
                return None;
            }
            let mut list = vec![music];
            list.extend(rules.command_channels.iter().copied().filter(|c| *c != music));
            Some(list)
        }
        None => (!channel_allowed(&rules.command_channels, channel, false)).then(|| rules.command_channels.clone()),
    }
}

/// Answer a slash command or button used outside the allowed text channels. Returns true when the
/// interaction was blocked (and answered).
pub async fn reject(lynx: &Lynx, http: &Http, interaction: &Interaction) -> bool {
    let (id, token, guild, channel, user, perms, command) = match interaction {
        Interaction::Command(c) => (
            c.id,
            c.token.as_str(),
            c.guild_id,
            c.channel_id.get(),
            c.user.id.get(),
            c.member.as_ref().and_then(|m| m.permissions),
            Some(c.data.name.as_str()),
        ),
        Interaction::Component(c) => (
            c.id,
            c.token.as_str(),
            c.guild_id,
            c.channel_id.get(),
            c.user.id.get(),
            c.member.as_ref().and_then(|m| m.permissions),
            None,
        ),
        _ => return false,
    };
    let Some(guild) = guild.map(|g| g.get()) else {
        return false;
    };
    let rules = lynx.store.channel_rules(guild).await.unwrap_or_default();
    let Some(allowed) = blocked(&rules, channel, command, false) else {
        return false;
    };
    let manager = respond::is_manager(&lynx.bots, guild, user, perms.map(|p| p.bits())).await;
    if blocked(&rules, channel, command, manager).is_none() {
        return false;
    }
    let msg = cards::error_key("command_channel", &[("channels", &mentions(&allowed))], &lynx.i18n);
    respond::respond(http, id, token, &msg.to_callback(callback::MESSAGE)).await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(commands: &[u64], music: Option<u64>) -> ChannelRules {
        ChannelRules {
            command_channels: commands.to_vec(),
            voice_channels: Vec::new(),
            music_channel: music,
            managers_exempt: true,
        }
    }

    #[test]
    fn no_rules_allow_everything() {
        assert_eq!(blocked(&rules(&[], None), 5, Some("play"), false), None);
        assert_eq!(blocked(&rules(&[], None), 5, None, false), None);
    }

    #[test]
    fn command_channels_only() {
        let r = rules(&[10, 11], None);
        assert_eq!(blocked(&r, 10, Some("play"), false), None);
        assert_eq!(blocked(&r, 5, Some("play"), false), Some(vec![10, 11]));
        assert_eq!(blocked(&r, 5, None, false), Some(vec![10, 11]), "buttons too");
    }

    #[test]
    fn music_channel_only() {
        let r = rules(&[], Some(40));
        assert_eq!(blocked(&r, 40, Some("play"), false), None);
        assert_eq!(blocked(&r, 40, None, false), None);
        assert_eq!(blocked(&r, 5, Some("play"), false), Some(vec![40]));
        assert_eq!(blocked(&r, 5, Some("about"), false), Some(vec![40]));
        assert_eq!(blocked(&r, 5, None, false), Some(vec![40]));
    }

    #[test]
    fn music_and_command_channels() {
        let r = rules(&[10, 40], Some(40));
        assert_eq!(blocked(&r, 40, Some("play"), false), None);
        assert_eq!(blocked(&r, 10, Some("play"), false), None);
        assert_eq!(
            blocked(&r, 5, Some("play"), false),
            Some(vec![40, 10]),
            "music first, no duplicates"
        );
    }

    #[test]
    fn invite_and_managers_are_never_blocked() {
        for r in [rules(&[10], None), rules(&[], Some(40)), rules(&[10], Some(40))] {
            assert_eq!(blocked(&r, 5, Some("invite"), false), None);
            assert_eq!(blocked(&r, 5, Some("play"), true), None);
            assert_eq!(blocked(&r, 5, None, true), None);
        }
    }

    #[test]
    fn managers_follow_the_rules_when_not_exempt() {
        for mut r in [rules(&[10], None), rules(&[], Some(10))] {
            r.managers_exempt = false;
            assert_eq!(blocked(&r, 5, Some("play"), true), Some(vec![10]));
            assert_eq!(blocked(&r, 5, None, true), Some(vec![10]));
            assert_eq!(blocked(&r, 10, Some("play"), true), None);
            assert_eq!(
                blocked(&r, 5, Some("invite"), true),
                None,
                "/invite still works anywhere"
            );
        }
    }
}
