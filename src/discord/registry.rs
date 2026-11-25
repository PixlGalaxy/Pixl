//! Shared view of every Lynx bot in this process: its HTTP client, cache and songbird, which guilds
//! it is in and which voice channel it sits in per guild. Also the bot assignment rule.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime},
};

use dashmap::DashMap;
use serenity::{
    all::{ChannelId, GuildId, UserId},
    cache::Cache,
    gateway::{ConnectionStage, ShardManager},
    http::Http,
    model::Permissions,
};
use songbird::Songbird;
use tokio::sync::{broadcast, Notify};

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
    /// The client's shard manager (gateway latency for `/about`).
    shards: OnceLock<Arc<ShardManager>>,
    /// Which shards have a live gateway session, and since when they do not (health endpoint).
    gateway: Mutex<GatewayTracker>,
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
            shards: OnceLock::new(),
            gateway: Mutex::new(GatewayTracker::new(Instant::now(), SystemTime::now())),
        }
    }

    /// A shard's gateway session came up (READY / RESUMED / stage Connected) or went away.
    pub fn set_shard_connected(&self, shard: u32, connected: bool) {
        let total = self.shards_total.load(Ordering::Relaxed);
        if let Ok(mut g) = self.gateway.lock() {
            g.set(shard, connected, total, Instant::now(), SystemTime::now());
        }
    }

    /// Gateway state now. The shard manager's own view of each shard is folded in first, so a
    /// shard that died without a stage event (runner restarted) is still seen.
    pub async fn gateway_status(&self) -> GatewayStatus {
        if let Some(shards) = self.shards.get() {
            let stages: Vec<(u32, bool)> = shards
                .runners
                .lock()
                .await
                .iter()
                .map(|(id, r)| (id.0, r.stage == ConnectionStage::Connected))
                .collect();
            let total = self.shards_total.load(Ordering::Relaxed);
            if let Ok(mut g) = self.gateway.lock() {
                g.reconcile(&stages, total, Instant::now(), SystemTime::now());
            }
        }
        let total = self.shards_total.load(Ordering::Relaxed);
        self.gateway
            .lock()
            .map(|g| g.status(total, Instant::now()))
            .unwrap_or_default()
    }

    pub fn set_shard_manager(&self, shards: Arc<ShardManager>) {
        let _ = self.shards.set(shards);
    }

    /// Change the presence on every shard of this bot (kept by serenity across reconnects).
    pub async fn set_presence(&self, activity: Option<serenity::all::ActivityData>, status: serenity::all::OnlineStatus) {
        let Some(shards) = self.shards.get() else { return };
        for runner in shards.runners.lock().await.values() {
            runner.runner_tx.set_presence(activity.clone(), status);
        }
    }

    /// Average heartbeat latency of the bot's shards; None until the first heartbeat is acknowledged.
    pub async fn gateway_latency(&self) -> Option<std::time::Duration> {
        let shards = self.shards.get()?;
        let runners = shards.runners.lock().await;
        let known: Vec<_> = runners.values().filter_map(|r| r.latency).collect();
        (!known.is_empty()).then(|| known.iter().sum::<std::time::Duration>() / known.len() as u32)
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

    /// Permissions of a member (from an interaction, roles included) in a guild channel, from this
    /// bot's cache. None when the guild or the channel is not cached.
    pub fn member_permissions_in(
        &self,
        guild_id: u64,
        channel_id: u64,
        member: &serenity::all::Member,
    ) -> Option<Permissions> {
        let guild = self.cache.guild(GuildId::new(guild_id))?;
        let channel = guild.channels.get(&ChannelId::new(channel_id))?;
        Some(guild.user_permissions_in(channel, member))
    }

    /// Can join and speak in the voice channel.
    pub fn can_play_in(&self, guild_id: u64, channel_id: u64) -> bool {
        self.channel_permissions(guild_id, channel_id)
            .is_some_and(|p| p.administrator() || (p.view_channel() && p.connect() && p.speak()))
    }
}

/// Gateway state of one bot, as the health endpoint reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GatewayStatus {
    pub shards_total: u32,
    pub shards_connected: u32,
    /// How long the bot has not had every shard connected (None: fully connected).
    pub degraded_for: Option<Duration>,
    /// How long the bot has had no shard connected at all (None: at least one is).
    pub down_for: Option<Duration>,
    /// Wall-clock start of the current outage (partial or total).
    pub down_since: Option<SystemTime>,
}

/// Per-shard connection state and outage timestamps. A bot starts disconnected (since creation),
/// so the startup handshake counts against the same grace period as a reconnect.
#[derive(Debug)]
struct GatewayTracker {
    up: HashMap<u32, bool>,
    /// Not every shard connected since (monotonic, wall clock).
    partial_since: Option<(Instant, SystemTime)>,
    /// No shard connected since.
    none_since: Option<Instant>,
}

impl GatewayTracker {
    fn new(now: Instant, wall: SystemTime) -> Self {
        Self {
            up: HashMap::new(),
            partial_since: Some((now, wall)),
            none_since: Some(now),
        }
    }

    fn set(&mut self, shard: u32, connected: bool, total: u32, now: Instant, wall: SystemTime) {
        self.up.insert(shard, connected);
        self.update(total, now, wall);
    }

    /// Fold in the shard manager's view: listed shards take its stage, shards it no longer runs
    /// are down.
    fn reconcile(&mut self, stages: &[(u32, bool)], total: u32, now: Instant, wall: SystemTime) {
        for v in self.up.values_mut() {
            *v = false;
        }
        for &(id, connected) in stages {
            self.up.insert(id, connected);
        }
        self.update(total, now, wall);
    }

    fn counts(&self, total: u32) -> (u32, u32) {
        let connected = self.up.values().filter(|v| **v).count() as u32;
        let total = total.max(self.up.len() as u32).max(1);
        (total, connected.min(total))
    }

    fn update(&mut self, total: u32, now: Instant, wall: SystemTime) {
        let (total, connected) = self.counts(total);
        if connected >= total {
            self.partial_since = None;
        } else if self.partial_since.is_none() {
            self.partial_since = Some((now, wall));
        }
        if connected > 0 {
            self.none_since = None;
        } else if self.none_since.is_none() {
            self.none_since = Some(now);
        }
    }

    fn status(&self, total: u32, now: Instant) -> GatewayStatus {
        let (total, connected) = self.counts(total);
        GatewayStatus {
            shards_total: total,
            shards_connected: connected,
            degraded_for: self.partial_since.map(|(at, _)| now.saturating_duration_since(at)),
            down_for: self.none_since.map(|at| now.saturating_duration_since(at)),
            down_since: self.partial_since.map(|(_, wall)| wall),
        }
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
    /// A human joined, left or moved between voice channels: (guild, user). The panel follows it.
    voice_moves: broadcast::Sender<(u64, u64)>,
}

impl BotRegistry {
    pub fn new(bots: Vec<Arc<BotInfo>>) -> Self {
        assert!(!bots.is_empty() && bots.len() <= 64, "1 main bot plus up to 63 nodes");
        Self {
            bots,
            presence: DashMap::new(),
            synced: Notify::new(),
            voice_moves: broadcast::channel(1024).0,
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

    // ---- human voice moves ------------------------------------------------

    /// A human's voice channel changed (joined, left or moved). Fired by the main bot only.
    pub fn voice_moved(&self, guild_id: u64, user_id: u64) {
        let _ = self.voice_moves.send((guild_id, user_id));
    }

    pub fn subscribe_voice_moves(&self) -> broadcast::Receiver<(u64, u64)> {
        self.voice_moves.subscribe()
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

    /// Humans in the channel who can hear the music: deafened users (self or server deafen) are left
    /// out, so they neither vote nor count toward the votes needed to skip.
    pub fn listeners_in(&self, guild_id: u64, channel_id: u64) -> HashSet<u64> {
        let humans = self.humans_in(guild_id, channel_id);
        let Some(guild) = self.main().cache.guild(GuildId::new(guild_id)) else {
            return humans;
        };
        humans
            .into_iter()
            .filter(|u| {
                guild
                    .voice_states
                    .get(&serenity::all::UserId::new(*u))
                    .is_none_or(|vs| !vs.self_deaf && !vs.deaf)
            })
            .collect()
    }

    pub fn guild_owner(&self, guild_id: u64) -> Option<u64> {
        self.main()
            .cache
            .guild(GuildId::new(guild_id))
            .map(|g| g.owner_id.get())
    }

    /// Name of a channel, from the first bot cache that has the guild.
    pub fn channel_name(&self, guild_id: u64, channel_id: u64) -> Option<String> {
        self.bots.iter().find_map(|b| {
            let g = b.cache.guild(GuildId::new(guild_id))?;
            g.channels.get(&ChannelId::new(channel_id)).map(|c| c.name.clone())
        })
    }

    /// Name of a guild, from the first bot cache that has it.
    pub fn guild_name(&self, guild_id: u64) -> Option<String> {
        self.bots
            .iter()
            .find_map(|b| b.cache.guild(GuildId::new(guild_id)).map(|g| g.name.clone()))
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

    fn registry(n: usize) -> BotRegistry {
        let bots = (0..n)
            .map(|i| {
                Arc::new(BotInfo::new(
                    i,
                    Arc::new(Http::new("x")),
                    Arc::new(Cache::new()),
                    { crate::test_support::songbird_scheduler(); Songbird::serenity() },
                ))
            })
            .collect();
        BotRegistry::new(bots)
    }

    #[tokio::test]
    async fn readiness_waits_for_every_shard_and_guild() {
        let r = registry(2);
        r.on_ready(0, 100, 1000, "Lynx", 2, &[1, 2]);
        r.on_ready(1, 101, 1001, "Lynx Node 1", 1, &[1]);
        assert!(!r.all_synced());
        r.on_guild_available(0, 1, None);
        r.on_guild_available(1, 1, Some(50));
        assert!(r.bot(1).unwrap().is_synced());
        r.on_guild_available(0, 2, None);
        assert!(!r.bot(0).unwrap().is_synced(), "second shard not ready yet");
        r.on_ready(0, 100, 1000, "Lynx", 2, &[]);
        assert!(r.all_synced());
        r.wait_synced(std::time::Duration::from_millis(10)).await;

        assert_eq!(r.index_of_user(101), Some(1));
        assert!(r.is_lynx_bot(100));
        assert!(!r.is_lynx_bot(0));
        assert_eq!(r.bot(1).unwrap().name(), "Lynx Node 1");
        assert!(r.bot(0).unwrap().invite_url().unwrap().ends_with("permissions=3148801"));
    }

    #[test]
    fn gateway_tracker_outages() {
        let t0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let s = |secs| t0 + Duration::from_secs(secs);
        let mut g = GatewayTracker::new(t0, w0);
        // Starts down (startup handshake).
        let st = g.status(0, s(5));
        assert_eq!((st.shards_total, st.shards_connected), (1, 0));
        assert_eq!(st.down_for, Some(Duration::from_secs(5)));
        assert_eq!(st.down_since, Some(w0));
        // Two shards: one up = partial outage, not down.
        g.set(0, true, 2, s(10), w0);
        let st = g.status(2, s(12));
        assert_eq!((st.shards_total, st.shards_connected), (2, 1));
        assert_eq!(st.down_for, None);
        assert_eq!(st.degraded_for, Some(Duration::from_secs(12)));
        // Both up: clear.
        g.set(1, true, 2, s(20), w0);
        assert_eq!(
            g.status(2, s(21)),
            GatewayStatus {
                shards_total: 2,
                shards_connected: 2,
                ..Default::default()
            }
        );
        // Shard 1 drops at 30 s; the outage starts then.
        let w30 = w0 + Duration::from_secs(30);
        g.set(1, false, 2, s(30), w30);
        let st = g.status(2, s(40));
        assert_eq!(st.degraded_for, Some(Duration::from_secs(10)));
        assert_eq!(st.down_since, Some(w30));
        // The manager no longer runs shard 0 either: fully down from the reconcile on.
        g.reconcile(&[(1, false)], 2, s(50), w30);
        let st = g.status(2, s(55));
        assert_eq!(st.shards_connected, 0);
        assert_eq!(st.down_for, Some(Duration::from_secs(5)));
        assert_eq!(st.down_since, Some(w30), "the outage started with the first shard");
        // Reconnect resets everything.
        g.reconcile(&[(0, true), (1, true)], 2, s(60), w30);
        let st = g.status(2, s(61));
        assert_eq!((st.degraded_for, st.down_for, st.down_since), (None, None, None));
    }

    #[tokio::test]
    async fn bot_info_gateway_events() {
        let r = registry(1);
        let bot = r.bot(0).unwrap();
        assert_eq!(bot.gateway_status().await.shards_connected, 0);
        r.on_ready(0, 100, 1000, "Lynx", 1, &[]);
        bot.set_shard_connected(0, true);
        let st = bot.gateway_status().await;
        assert_eq!((st.shards_total, st.shards_connected, st.down_since), (1, 1, None));
        bot.set_shard_connected(0, false);
        assert!(bot.gateway_status().await.down_for.is_some());
    }

    #[tokio::test]
    async fn presence_tracking() {
        let r = registry(3);
        r.on_ready(1, 101, 1001, "Node 1", 1, &[]);
        r.on_ready(2, 102, 1002, "Node 2", 1, &[]);
        r.set_member(7, 0, true);
        r.set_member(7, 1, true);
        r.set_voice(7, 1, Some(70));
        assert!(r.main_in_guild(7));
        assert_eq!(r.voice_of(7, 1), Some(70));
        assert_eq!(r.bot_in_channel(7, 70), Some(1));
        assert_eq!(r.all_voice(), vec![(7, 1, 70)]);
        assert_eq!(r.guilds_of(1), vec![7]);
        // Node 2 is the next one to invite.
        assert_eq!(r.next_node_invite(7).map(|(n, _)| n), Some(2));
        r.set_member(7, 2, true);
        assert_eq!(r.next_node_invite(7), None);
        // Leaving a guild clears its voice state; the guild entry goes away with the last bot.
        r.set_member(7, 1, false);
        assert_eq!(r.voice_of(7, 1), None);
        r.set_member(7, 0, false);
        r.set_member(7, 2, false);
        assert!(!r.in_guild(7, 0));
        assert!(r.presence.is_empty());
        // No cache data: nobody in voice, no permissions.
        assert!(r.humans_in(7, 70).is_empty());
        assert!(!r.bot(0).unwrap().can_play_in(7, 70));
    }
}
