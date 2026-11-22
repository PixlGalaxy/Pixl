//! Shared view of every Lynx bot in this process: its HTTP client, cache and songbird, which guilds
//! it is in and which voice channel it sits in per guild. Also the bot assignment rule.

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use dashmap::DashMap;
use serenity::{
    all::{ChannelId, GuildId, UserId},
    cache::Cache,
    http::Http,
    model::Permissions,
};
use songbird::Songbird;
use tokio::sync::Notify;

/// Permissions requested by invite links: View Channel, Send Messages, Connect, Speak, plus Create
/// Invite for the main bot ("Invite to jam").
pub const NODE_INVITE_PERMISSIONS: u64 = (1 << 10) | (1 << 11) | (1 << 20) | (1 << 21);
pub const MAIN_INVITE_PERMISSIONS: u64 = NODE_INVITE_PERMISSIONS | 1;

pub fn invite_url(application_id: u64, permissions: u64) -> String {
    format!(
        "https://discord.com/oauth2/authorize?client_id={application_id}&scope=bot%20applications.commands&permissions={permissions}"
    )
}

/// One bot (main = index 0, nodes = 1..).
pub struct BotInfo {
    pub index: usize,
    pub http: Arc<Http>,
    pub cache: Arc<Cache>,
    pub songbird: Arc<Songbird>,
    user_id: AtomicU64,
    application_id: AtomicU64,
    name: OnceLock<String>,
    shards_total: AtomicU32,
    shards_ready: AtomicU32,
    /// Guilds announced in READY that have not arrived yet (GUILD_CREATE).
    pending: Mutex<HashSet<u64>>,
    synced: AtomicBool,
}

impl BotInfo {
    pub fn new(index: usize, http: Arc<Http>, cache: Arc<Cache>, songbird: Arc<Songbird>) -> Self {
        Self {
            index,
            http,
            cache,
            songbird,
            user_id: AtomicU64::new(0),
            application_id: AtomicU64::new(0),
            name: OnceLock::new(),
            shards_total: AtomicU32::new(0),
            shards_ready: AtomicU32::new(0),
            pending: Mutex::new(HashSet::new()),
            synced: AtomicBool::new(false),
        }
    }

    pub fn is_main(&self) -> bool {
        self.index == 0
    }

    pub fn user_id(&self) -> u64 {
        self.user_id.load(Ordering::Relaxed)
    }

    pub fn application_id(&self) -> u64 {
        self.application_id.load(Ordering::Relaxed)
    }

    pub fn name(&self) -> &str {
        self.name.get().map(String::as_str).unwrap_or("Lynx")
    }

    pub fn is_ready(&self) -> bool {
        self.user_id() != 0
    }

    pub fn is_synced(&self) -> bool {
        self.synced.load(Ordering::Acquire)
    }

    pub fn invite_url(&self) -> Option<String> {
        let app = self.application_id();
        (app != 0).then(|| {
            invite_url(
                app,
                if self.is_main() {
                    MAIN_INVITE_PERMISSIONS
                } else {
                    NODE_INVITE_PERMISSIONS
                },
            )
        })
    }

    /// Permissions of this bot in a guild channel, from its own cache.
    pub fn channel_permissions(&self, guild_id: u64, channel_id: u64) -> Option<Permissions> {
        let guild = self.cache.guild(GuildId::new(guild_id))?;
        let channel = guild.channels.get(&ChannelId::new(channel_id))?;
        let me = guild.members.get(&UserId::new(self.user_id()))?;
        Some(guild.user_permissions_in(channel, me))
    }

    /// Can join and speak in the voice channel.
    pub fn can_play_in(&self, guild_id: u64, channel_id: u64) -> bool {
        self.channel_permissions(guild_id, channel_id)
            .is_some_and(|p| p.administrator() || (p.view_channel() && p.connect() && p.speak()))
    }
}

#[derive(Debug, Clone, Default)]
struct GuildBots {
    /// Bit i set = bot i is a member.
    members: u64,
    /// (bot index, voice channel id).
    voice: Vec<(usize, u64)>,
}

/// Registry of every bot. Built once before the clients start; presence is updated by gateway events.
pub struct BotRegistry {
    bots: Vec<Arc<BotInfo>>,
    presence: DashMap<u64, GuildBots>,
    synced: Notify,
}

impl BotRegistry {
    pub fn new(bots: Vec<Arc<BotInfo>>) -> Self {
        assert!(!bots.is_empty() && bots.len() <= 64, "1 main bot plus up to 63 nodes");
        Self {
            bots,
            presence: DashMap::new(),
            synced: Notify::new(),
        }
    }

    pub fn main(&self) -> &Arc<BotInfo> {
        &self.bots[0]
    }

    pub fn bot(&self, index: usize) -> Option<&Arc<BotInfo>> {
        self.bots.get(index)
    }

    pub fn bots(&self) -> &[Arc<BotInfo>] {
        &self.bots
    }

    pub fn nodes(&self) -> &[Arc<BotInfo>] {
        &self.bots[1..]
    }

    /// Index of the bot with this user id.
    pub fn index_of_user(&self, user_id: u64) -> Option<usize> {
        self.bots.iter().position(|b| b.user_id() == user_id && user_id != 0)
    }

    pub fn is_lynx_bot(&self, user_id: u64) -> bool {
        self.index_of_user(user_id).is_some()
    }

    // ---- readiness --------------------------------------------------------

    /// READY of one shard: remember the bot identity and the guilds still to arrive.
    pub fn on_ready(&self, index: usize, user_id: u64, application_id: u64, name: &str, shards: u32, guilds: &[u64]) {
        let Some(bot) = self.bots.get(index) else { return };
        bot.user_id.store(user_id, Ordering::Relaxed);
        bot.application_id.store(application_id, Ordering::Relaxed);
        let _ = bot.name.set(name.to_owned());
        bot.shards_total.store(shards.max(1), Ordering::Relaxed);
        bot.shards_ready.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut p) = bot.pending.lock() {
            p.extend(guilds.iter().copied());
        }
        self.check_synced(index);
    }

    /// A guild arrived (startup or a new join).
    pub fn on_guild_available(&self, index: usize, guild_id: u64, voice_channel: Option<u64>) {
        self.set_member(guild_id, index, true);
        self.set_voice(guild_id, index, voice_channel);
        if let Some(bot) = self.bots.get(index) {
            if let Ok(mut p) = bot.pending.lock() {
                p.remove(&guild_id);
            }
        }
        self.check_synced(index);
    }

    /// A guild that never arrives (unavailable) must not block startup forever.
    pub fn force_synced(&self) {
        for b in &self.bots {
            b.synced.store(true, Ordering::Release);
        }
        self.synced.notify_waiters();
    }

    fn check_synced(&self, index: usize) {
        let Some(bot) = self.bots.get(index) else { return };
        let total = bot.shards_total.load(Ordering::Relaxed);
        let ready = bot.shards_ready.load(Ordering::Acquire);
        let empty = bot.pending.lock().map(|p| p.is_empty()).unwrap_or(true);
        if total > 0 && ready >= total && empty && !bot.synced.swap(true, Ordering::AcqRel) {
            self.synced.notify_waiters();
        }
    }

    pub fn all_synced(&self) -> bool {
        self.bots.iter().all(|b| b.is_synced())
    }

    /// Wait until every bot received all its guilds (or `timeout`, then force it).
    pub async fn wait_synced(&self, timeout: std::time::Duration) {
        let wait = async {
            loop {
                let notified = self.synced.notified();
                if self.all_synced() {
                    return;
                }
                notified.await;
            }
        };
        if tokio::time::timeout(timeout, wait).await.is_err() {
            tracing::warn!("not every bot finished loading its guilds in time; continuing");
            self.force_synced();
        }
    }

    // ---- guild data from the main bot's cache ----------------------------------------------

    /// Voice channel a user is in, from the main bot's cache.
    pub fn user_voice_channel(&self, guild_id: u64, user_id: u64) -> Option<u64> {
        let guild = self.main().cache.guild(GuildId::new(guild_id))?;
        guild
            .voice_states
            .get(&UserId::new(user_id))?
            .channel_id
            .map(|c| c.get())
    }

    /// Humans (not bots) in a voice channel, from the main bot's cache.
    pub fn humans_in(&self, guild_id: u64, channel_id: u64) -> HashSet<u64> {
        let Some(guild) = self.main().cache.guild(GuildId::new(guild_id)) else {
            return HashSet::new();
        };
        guild
            .voice_states
            .values()
            .filter(|vs| vs.channel_id.map(|c| c.get()) == Some(channel_id))
            .filter(|vs| {
                let bot = vs
                    .member
                    .as_ref()
                    .map(|m| m.user.bot)
                    .or_else(|| guild.members.get(&vs.user_id).map(|m| m.user.bot))
                    .unwrap_or(false);
                !bot && !self.is_lynx_bot(vs.user_id.get())
            })
            .map(|vs| vs.user_id.get())
            .collect()
    }

    pub fn guild_owner(&self, guild_id: u64) -> Option<u64> {
        self.main()
            .cache
            .guild(GuildId::new(guild_id))
            .map(|g| g.owner_id.get())
    }

    /// The voice channel still exists (main bot's cache).
    pub fn channel_exists(&self, guild_id: u64, channel_id: u64) -> bool {
        self.main()
            .cache
            .guild(GuildId::new(guild_id))
            .is_some_and(|g| g.channels.contains_key(&ChannelId::new(channel_id)))
    }

    // ---- presence ---------------------------------------------------------

    pub fn set_member(&self, guild_id: u64, index: usize, member: bool) {
        let mut g = self.presence.entry(guild_id).or_default();
        if member {
            g.members |= 1 << index;
        } else {
            g.members &= !(1 << index);
            g.voice.retain(|(i, _)| *i != index);
        }
        let empty = g.members == 0;
        drop(g);
        if empty {
            self.presence.remove_if(&guild_id, |_, g| g.members == 0);
        }
    }

    pub fn set_voice(&self, guild_id: u64, index: usize, channel: Option<u64>) {
        let Some(mut g) = self.presence.get_mut(&guild_id) else {
            return;
        };
        g.voice.retain(|(i, _)| *i != index);
        if let Some(c) = channel {
            g.voice.push((index, c));
        }
    }

    pub fn in_guild(&self, guild_id: u64, index: usize) -> bool {
        self.presence
            .get(&guild_id)
            .is_some_and(|g| g.members & (1 << index) != 0)
    }

    pub fn main_in_guild(&self, guild_id: u64) -> bool {
        self.in_guild(guild_id, 0)
    }

    pub fn voice_of(&self, guild_id: u64, index: usize) -> Option<u64> {
        self.presence
            .get(&guild_id)?
            .voice
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, c)| *c)
    }

    /// Bot sitting in this voice channel, if any.
    pub fn bot_in_channel(&self, guild_id: u64, channel_id: u64) -> Option<usize> {
        self.presence
            .get(&guild_id)?
            .voice
            .iter()
            .find(|(_, c)| *c == channel_id)
            .map(|(i, _)| *i)
    }

    /// Every (bot, voice channel) pair currently known (startup cleanup).
    pub fn all_voice(&self) -> Vec<(u64, usize, u64)> {
        self.presence
            .iter()
            .flat_map(|g| g.voice.iter().map(|(i, c)| (*g.key(), *i, *c)).collect::<Vec<_>>())
            .collect()
    }

    /// Guilds a bot is in.
    pub fn guilds_of(&self, index: usize) -> Vec<u64> {
        self.presence
            .iter()
            .filter(|g| g.members & (1 << index) != 0)
            .map(|g| *g.key())
            .collect()
    }

    /// First node (in `NODE_TOKENS` order) that is not in the guild yet: (node number, invite URL).
    pub fn next_node_invite(&self, guild_id: u64) -> Option<(usize, String)> {
        self.nodes()
            .iter()
            .find(|b| !self.in_guild(guild_id, b.index))
            .and_then(|b| b.invite_url().map(|u| (b.index, u)))
    }

    /// Candidates for the assignment rule, from presence and each bot's own cache.
    pub fn candidates(&self, guild_id: u64, channel_id: u64, busy: impl Fn(usize) -> bool) -> Vec<Candidate> {
        self.bots
            .iter()
            .map(|b| Candidate {
                index: b.index,
                ready: b.is_ready(),
                in_guild: self.in_guild(guild_id, b.index),
                voice: self.voice_of(guild_id, b.index),
                has_player: busy(b.index),
                can_play: b.can_play_in(guild_id, channel_id),
            })
            .collect()
    }
}

/// What the assignment rule needs to know about one bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub index: usize,
    pub ready: bool,
    pub in_guild: bool,
    /// Voice channel the bot is in, in this guild.
    pub voice: Option<u64>,
    /// The bot already runs a player in this guild (it may still be joining).
    pub has_player: bool,
    pub can_play: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assignment {
    /// A Lynx bot is already in the user's channel.
    Existing(usize),
    /// First free bot: main first, then nodes in order.
    Free(usize),
    /// A bot is free but lacks Connect/Speak in that channel.
    CannotJoin,
    /// Every bot in the guild is busy elsewhere.
    Busy { busy_channel: Option<u64> },
}

/// The assignment rule (plan: README "Asignación al hacer /play").
pub fn assign(candidates: &[Candidate], channel_id: u64) -> Assignment {
    if let Some(c) = candidates.iter().find(|c| c.in_guild && c.voice == Some(channel_id)) {
        return Assignment::Existing(c.index);
    }
    let free = |c: &&Candidate| c.ready && c.in_guild && c.voice.is_none() && !c.has_player;
    if let Some(c) = candidates.iter().filter(free).find(|c| c.can_play) {
        return Assignment::Free(c.index);
    }
    if candidates.iter().any(|c| free(&c)) {
        return Assignment::CannotJoin;
    }
    Assignment::Busy {
        busy_channel: candidates.iter().find(|c| c.in_guild).and_then(|c| c.voice),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(index: usize, in_guild: bool, voice: Option<u64>) -> Candidate {
        Candidate {
            index,
            ready: true,
            in_guild,
            voice,
            has_player: voice.is_some(),
            can_play: true,
        }
    }

    #[test]
    fn assignment_rules() {
        // Main free: main plays.
        assert_eq!(assign(&[c(0, true, None), c(1, true, None)], 10), Assignment::Free(0));
        // Main busy elsewhere: first node.
        assert_eq!(
            assign(&[c(0, true, Some(5)), c(1, true, None), c(2, true, None)], 10),
            Assignment::Free(1)
        );
        // Node 1 not in the guild: node 2.
        assert_eq!(
            assign(&[c(0, true, Some(5)), c(1, false, None), c(2, true, None)], 10),
            Assignment::Free(2)
        );
        // A bot already in the user's channel wins, even a node.
        assert_eq!(
            assign(&[c(0, true, None), c(1, true, Some(10))], 10),
            Assignment::Existing(1)
        );
        // Everyone busy.
        assert_eq!(
            assign(&[c(0, true, Some(5)), c(1, false, None)], 10),
            Assignment::Busy { busy_channel: Some(5) }
        );
        // Free bot without permission.
        let mut no_perm = c(0, true, None);
        no_perm.can_play = false;
        assert_eq!(assign(&[no_perm, c(1, false, None)], 10), Assignment::CannotJoin);
        // ...but a later node with permission is used.
        assert_eq!(assign(&[no_perm, c(1, true, None)], 10), Assignment::Free(1));
        // A bot that is still joining (player, no voice state yet) is not free.
        let mut joining = c(0, true, None);
        joining.has_player = true;
        assert_eq!(assign(&[joining, c(1, true, None)], 10), Assignment::Free(1));
        // Not ready bots are skipped.
        let mut not_ready = c(1, true, None);
        not_ready.ready = false;
        assert_eq!(
            assign(&[c(0, true, Some(1)), not_ready], 10),
            Assignment::Busy { busy_channel: Some(1) }
        );
    }

    #[test]
    fn invite_links() {
        let url = invite_url(123, MAIN_INVITE_PERMISSIONS);
        assert_eq!(
            url,
            "https://discord.com/oauth2/authorize?client_id=123&scope=bot%20applications.commands&permissions=3148801"
        );
        assert_eq!(NODE_INVITE_PERMISSIONS, 3148800);
    }
}
