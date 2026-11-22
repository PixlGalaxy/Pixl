//! Discord side: the main bot (commands, messages) plus voice-only node bots, all in this process.
//!
//! Every bot has its own serenity client (autosharded, intents GUILDS | GUILD_VOICE_STATES) and its
//! own songbird. [`registry::BotRegistry`] keeps who is where; [`Lynx`] is the shared context the
//! event handlers use.

pub mod commands;
pub mod ids;
pub mod interactions;
pub mod jam;
pub mod registry;
pub mod respond;
pub mod resume;
pub mod ui;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};

use anyhow::Result;
use serenity::{
    all::{Context, EventHandler, GatewayIntents, GuildChannel, Interaction, Ready, UnavailableGuild, VoiceState},
    async_trait,
    model::guild::Guild,
    Client,
};
use songbird::{SerenityInit, Songbird};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::{
    config::Config,
    i18n::I18n,
    player::{EndReason, PlayerManager},
    sources::Sources,
    store::Store,
};
use jam::JamInvites;
use registry::{BotInfo, BotRegistry, MAIN_INVITE_PERMISSIONS};

/// Shared context of the Discord layer.
pub struct Lynx {
    pub config: Arc<Config>,
    pub i18n: Arc<I18n>,
    pub store: Store,
    pub sources: Sources,
    pub bots: Arc<BotRegistry>,
    pub players: PlayerManager,
    pub jam: JamInvites,
    /// Startup finished (guilds loaded, nodes reconciled, sessions resumed).
    pub booted: AtomicBool,
}

impl Lynx {
    /// Invite link of the main bot.
    pub fn main_invite(&self) -> String {
        self.bots.main().invite_url().unwrap_or_else(|| {
            registry::invite_url(
                self.config.discord_client_id.parse().unwrap_or(0),
                MAIN_INVITE_PERMISSIONS,
            )
        })
    }

    pub fn is_booted(&self) -> bool {
        self.booted.load(Ordering::Acquire)
    }
}

type Slot = Arc<OnceLock<Arc<Lynx>>>;

/// The bot clients, built before the player layer so it can use their HTTP clients and songbirds.
pub struct Bots {
    pub registry: Arc<BotRegistry>,
    clients: Vec<Client>,
    slot: Slot,
    /// Stops the gateways (after the players left voice).
    pub stop: CancellationToken,
}

impl Bots {
    pub async fn build(config: &Config) -> Result<Self> {
        let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
        let slot: Slot = Arc::new(OnceLock::new());
        let tokens = std::iter::once(&config.discord_token).chain(config.node_tokens.iter());
        let mut clients = Vec::new();
        let mut infos = Vec::new();
        for (index, token) in tokens.enumerate() {
            let songbird = Songbird::serenity();
            let client = Client::builder(token, intents)
                .event_handler(Handler {
                    index,
                    slot: slot.clone(),
                    registered: AtomicBool::new(false),
                })
                .register_songbird_with(songbird.clone())
                .await?;
            infos.push(Arc::new(BotInfo::new(
                index,
                client.http.clone(),
                client.cache.clone(),
                songbird,
            )));
            clients.push(client);
        }
        Ok(Self {
            registry: Arc::new(BotRegistry::new(infos)),
            clients,
            slot,
            stop: CancellationToken::new(),
        })
    }

    /// Connect every bot and run the startup sequence. The task ends when `stop` is cancelled.
    pub fn start(self, lynx: Arc<Lynx>) -> JoinHandle<()> {
        let _ = self.slot.set(lynx.clone());
        let stop = self.stop.clone();
        tokio::spawn(resume::boot(lynx));
        tokio::spawn(async move {
            let mut tasks = Vec::new();
            for (index, mut client) in self.clients.into_iter().enumerate() {
                let shards = client.shard_manager.clone();
                let stop = stop.clone();
                tasks.push(tokio::spawn(async move {
                    tokio::select! {
                        res = client.start_autosharded() => if let Err(e) = res {
                            error!(bot = index, error = %e, "discord client stopped");
                        },
                        _ = stop.cancelled() => shards.shutdown_all().await,
                    }
                }));
            }
            for t in tasks {
                let _ = t.await;
            }
        })
    }
}

struct Handler {
    index: usize,
    slot: Slot,
    registered: AtomicBool,
}

impl Handler {
    fn lynx(&self) -> Option<&Arc<Lynx>> {
        self.slot.get()
    }

    fn me(&self, lynx: &Lynx) -> Option<Arc<BotInfo>> {
        lynx.bots.bot(self.index).cloned()
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        let Some(lynx) = self.lynx() else { return };
        let shards = ready.shard.map(|s| s.total).unwrap_or(1);
        let guilds: Vec<u64> = ready.guilds.iter().map(|g| g.id.get()).collect();
        info!(bot = self.index, user = %ready.user.name, guilds = guilds.len(), "bot ready");
        lynx.bots.on_ready(
            self.index,
            ready.user.id.get(),
            ready.application.id.get(),
            &ready.user.name,
            shards,
            &guilds,
        );
        if self.index == 0 && !self.registered.swap(true, Ordering::AcqRel) {
            let lynx = lynx.clone();
            tokio::spawn(async move { commands::register_main(&lynx).await });
        }
    }

    async fn guild_create(&self, _ctx: Context, guild: Guild, is_new: Option<bool>) {
        let Some(lynx) = self.lynx() else { return };
        let Some(me) = self.me(lynx) else { return };
        let g = guild.id.get();
        let own_voice = guild
            .voice_states
            .get(&serenity::all::UserId::new(me.user_id().max(1)))
            .and_then(|vs| vs.channel_id)
            .map(|c| c.get());
        lynx.bots.on_guild_available(self.index, g, own_voice);
        if !lynx.is_booted() {
            return; // startup reconciliation covers it
        }
        let joined = is_new == Some(true);
        if self.index == 0 {
            if joined {
                // The main bot arrived: nodes drop their guild commands here.
                for node in lynx.bots.nodes().iter().filter(|n| lynx.bots.in_guild(g, n.index)) {
                    commands::sync_node_guild(lynx, node, g, false).await;
                }
            }
        } else {
            commands::sync_node_guild(lynx, &me, g, joined).await;
        }
    }

    async fn guild_delete(&self, _ctx: Context, incomplete: UnavailableGuild, _full: Option<Guild>) {
        let Some(lynx) = self.lynx() else { return };
        if incomplete.unavailable {
            return; // outage, the guild comes back
        }
        let g = incomplete.id.get();
        lynx.bots.set_member(g, self.index, false);
        for p in lynx
            .players
            .in_guild(g)
            .into_iter()
            .filter(|p| p.bot_index == self.index)
        {
            let _ = p.end(EndReason::GuildGone).await;
        }
        if !lynx.is_booted() {
            return;
        }
        if self.index == 0 {
            // The main bot left: nodes still there register their commands again.
            for node in lynx.bots.nodes().iter().filter(|n| lynx.bots.in_guild(g, n.index)) {
                commands::sync_node_guild(lynx, node, g, false).await;
            }
        } else if let Some(me) = self.me(lynx) {
            let _ = lynx.store.delete_node_guild_commands(me.application_id(), g).await;
        }
    }

    async fn channel_delete(
        &self,
        _ctx: Context,
        channel: GuildChannel,
        _messages: Option<Vec<serenity::all::Message>>,
    ) {
        let Some(lynx) = self.lynx() else { return };
        if self.index != 0 {
            return;
        }
        if let Some(p) = lynx.players.get(channel.guild_id.get(), channel.id.get()) {
            let _ = p.end(EndReason::ChannelDeleted).await;
        }
    }

    async fn voice_state_update(&self, _ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        let Some(lynx) = self.lynx() else { return };
        let Some(guild) = new.guild_id else { return };
        let g = guild.get();
        let user = new.user_id.get();
        let new_channel = new.channel_id.map(|c| c.get());
        let Some(me) = self.me(lynx) else { return };
        if user == me.user_id() {
            lynx.bots.set_voice(g, self.index, new_channel);
            own_voice_state(lynx, self.index, g, new_channel).await;
            return;
        }
        // Humans are tracked through the main bot only (every bot sees the same updates).
        if self.index != 0 || lynx.bots.is_lynx_bot(user) {
            return;
        }
        let old_channel = old.as_ref().and_then(|o| o.channel_id).map(|c| c.get());
        if old.is_some() && old_channel == new_channel {
            return; // mute / deafen / stream change
        }
        let targets = if old.is_none() {
            lynx.players.in_guild(g)
        } else {
            [old_channel, new_channel]
                .into_iter()
                .flatten()
                .filter_map(|c| lynx.players.get(g, c))
                .collect()
        };
        for p in targets {
            let humans = lynx.bots.humans_in(g, p.voice_channel_id());
            let _ = p.humans_changed(humans).await;
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Some(lynx) = self.lynx().cloned() else { return };
        if self.index == 0 {
            interactions::handle(lynx, ctx.http.clone(), interaction).await;
        } else {
            interactions::handle_node(lynx, ctx.http.clone(), interaction).await;
        }
    }
}

/// A Lynx bot's own voice state changed: kicked, moved, or (re)joined.
async fn own_voice_state(lynx: &Lynx, index: usize, guild_id: u64, channel: Option<u64>) {
    if lynx.players.is_closing() {
        return;
    }
    let Some(player) = lynx
        .players
        .in_guild(guild_id)
        .into_iter()
        .find(|p| p.bot_index == index)
    else {
        return;
    };
    match channel {
        None => {
            info!(guild = guild_id, bot = index, "bot was disconnected from voice");
            let _ = player.end(EndReason::Kicked).await;
        }
        Some(c) if c != player.voice_channel_id() => {
            if lynx.players.rekey(&player, c) {
                let _ = player.moved(c).await;
                let _ = player.humans_changed(lynx.bots.humans_in(guild_id, c)).await;
            } else {
                // Dragged into a channel that already has a Lynx player.
                let _ = player.end(EndReason::Kicked).await;
            }
        }
        Some(_) => {}
    }
}
