//! Interaction router of the main bot: slash commands, autocomplete and buttons.
//! Every command acts on the player of the user's current voice channel.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use serenity::{
    all::{CommandDataOptionValue, CommandInteraction, ComponentInteraction, Interaction, InteractionId, Member, User},
    http::Http,
};
use tracing::{debug, warn};

use super::{
    ids::{track_tag, CustomId, NpAction, QUEUE_BUTTONS_TTL_SECS},
    play::{self, PlayCtx, PlayError, PlayRequest, Played},
    radio::{self as radio_ui, RadioCtx, RadioForm},
    respond::{self, actor},
    ui::{
        self, bare_callback, callback,
        cards::{self, Tone, TrackCard, PLAYLIST_PREVIEW, QUEUE_PAGE_SIZE},
        defer_callback, link, truncate, Message,
    },
    Lynx,
};
use crate::{
    perms::{can, can_jam, Action, Decision, Deny, JamAction},
    player::{jam::CreateRefused, Entry, PlayerHandle, PlayerSnapshot, QueueEntry, Requester, Restore, SkipOutcome},
    sources::{SourceError, TrackInfo},
    store::{now_secs, Feedback, GuildSettings, LoopMode},
};

const AUTOCOMPLETE_TIMEOUT: Duration = Duration::from_millis(2500);

/// One interaction being answered. Tracks whether the response was deferred.
struct Inv<'a> {
    lynx: &'a Lynx,
    http: &'a Http,
    id: InteractionId,
    token: &'a str,
    guild_id: u64,
    user_id: u64,
    text_channel: u64,
    permissions: Option<u64>,
    name: String,
    avatar: Option<String>,
    deferred: AtomicBool,
}

impl<'a> Inv<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        lynx: &'a Lynx,
        http: &'a Http,
        id: InteractionId,
        token: &'a str,
        guild: Option<u64>,
        channel: u64,
        member: Option<&Member>,
        user: &User,
    ) -> Self {
        Self {
            lynx,
            http,
            id,
            token,
            guild_id: guild.unwrap_or(0),
            user_id: user.id.get(),
            text_channel: channel,
            permissions: member.and_then(|m| m.permissions).map(|p| p.bits()),
            name: member
                .and_then(|m| m.nick.clone())
                .or_else(|| user.global_name.clone())
                .unwrap_or_else(|| user.name.clone()),
            avatar: Some(member.map(|m| m.face()).unwrap_or_else(|| user.face())),
            deferred: AtomicBool::new(false),
        }
    }

    fn t(&self, key: &str, args: &[(&str, &str)]) -> String {
        self.lynx.i18n.t(key, args)
    }

    fn err(&self, key: &str, args: &[(&str, &str)]) -> Message {
        cards::error_key(key, args, &self.lynx.i18n)
    }

    async fn defer(&self, ephemeral: bool) {
        if respond::respond(self.http, self.id, self.token, &defer_callback(ephemeral)).await {
            self.deferred.store(true, Ordering::Release);
        }
    }

    /// Send the final message (new response, or edit of the deferred one; an ephemeral message after
    /// a public deferral replaces it with an ephemeral followup).
    async fn send(&self, msg: Message) {
        if !self.deferred.load(Ordering::Acquire) {
            respond::respond(self.http, self.id, self.token, &msg.to_callback(callback::MESSAGE)).await;
        } else if msg.ephemeral {
            respond::delete_original(self.http, self.token).await;
            respond::followup(self.http, self.token, &msg.to_json()).await;
        } else {
            respond::edit_original(self.http, self.token, &msg.to_json()).await;
        }
    }

    async fn callback(&self, body: serde_json::Value) {
        respond::respond(self.http, self.id, self.token, &body).await;
    }

    fn user_voice(&self) -> Option<u64> {
        self.lynx.bots.user_voice_channel(self.guild_id, self.user_id)
    }

    fn requester(&self) -> Requester {
        Requester {
            id: self.user_id,
            name: Some(self.name.clone()),
            avatar_url: self.avatar.clone(),
        }
    }

    async fn settings(&self) -> GuildSettings {
        self.lynx.store.guild_settings(self.guild_id).await.unwrap_or_default()
    }

    /// Player for an action: the one in the user's voice channel (or, for read-only actions, the
    /// only player of the guild). Checks the permission rule.
    async fn player_for(&self, action: Action) -> Result<(PlayerHandle, GuildSettings, Decision), Message> {
        self.player_for_in(action, true).await
    }

    /// `player_for` for per-channel actions (volume, stop): the jam's edit rule does not apply.
    async fn player_for_local(&self, action: Action) -> Result<(PlayerHandle, GuildSettings, Decision), Message> {
        self.player_for_in(action, false).await
    }

    async fn player_for_in(
        &self,
        action: Action,
        shared: bool,
    ) -> Result<(PlayerHandle, GuildSettings, Decision), Message> {
        let player = match self.user_voice() {
            Some(vc) => self.lynx.players.get(self.guild_id, vc),
            None if action == Action::View => {
                let mut all = self.lynx.players.in_guild(self.guild_id);
                if all.len() == 1 {
                    all.pop()
                } else {
                    return Err(self.err("not_in_voice", &[]));
                }
            }
            None => return Err(self.err("not_in_voice", &[])),
        };
        let Some(player) = player else {
            return Err(self.err("no_player", &[]));
        };
        self.check_in(player, action, shared).await
    }

    async fn check(
        &self,
        player: PlayerHandle,
        action: Action,
    ) -> Result<(PlayerHandle, GuildSettings, Decision), Message> {
        self.check_in(player, action, true).await
    }

    /// The permission rule, plus (`shared`) the jam rule for actions on the shared queue.
    async fn check_in(
        &self,
        player: PlayerHandle,
        action: Action,
        shared: bool,
    ) -> Result<(PlayerHandle, GuildSettings, Decision), Message> {
        let settings = self.settings().await;
        let snap = player.snapshot();
        let requested_current = snap.current.as_ref().is_some_and(|e| e.requester.id == self.user_id);
        let a = actor(
            &self.lynx.bots,
            &self.lynx.store,
            self.guild_id,
            self.user_id,
            self.permissions,
            player.voice_channel_id(),
            requested_current,
        )
        .await;
        match can(&a, action, &settings) {
            Decision::Deny(d) => Err(self.deny(d)),
            decision => {
                if shared && !matches!(action, Action::View | Action::Feedback) {
                    self.jam_check(&player)?;
                }
                Ok((player, settings, decision))
            }
        }
    }

    /// In a jam that only the host channel edits, other channels may not touch the shared queue.
    fn jam_check(&self, player: &PlayerHandle) -> Result<(), Message> {
        let key = player.key();
        match self.lynx.players.jams().of(key) {
            Some(j) if !j.view().may_edit(key) => {
                let host = j.view().host_member().map(|m| m.name.clone()).unwrap_or_default();
                Err(self.err(
                    "jam_host_only",
                    &[("channel", &format!("**{}**", ui::escape(&ui::truncate(&host, 80))))],
                ))
            }
            _ => Ok(()),
        }
    }

    fn deny(&self, d: Deny) -> Message {
        match d {
            Deny::NotInChannel => self.err("not_in_channel", &[]),
            Deny::DjOnly => self.err("dj_only", &[]),
            Deny::ManagerOnly => self.err("manager_only", &[]),
        }
    }
}

fn radio_ctx<'a>(inv: &'a Inv<'a>) -> RadioCtx<'a> {
    RadioCtx {
        lynx: inv.lynx,
        http: inv.http,
        id: inv.id,
        token: inv.token,
        guild_id: inv.guild_id,
        user: inv.requester(),
        text_channel: inv.text_channel,
        permissions: inv.permissions,
        user_voice: inv.user_voice(),
    }
}

fn gone(inv: &Inv) -> Message {
    inv.err("no_player", &[])
}

fn card<'e>(e: &'e QueueEntry) -> TrackCard<'e> {
    TrackCard {
        title: &e.info.title,
        url: &e.info.url,
        artists: &e.info.artists,
        duration_ms: e.info.duration_ms,
        thumbnail: e.info.thumbnail.as_deref(),
        requester_id: e.requester.id,
        ai: e.reason.is_some(),
    }
}

fn track_link(e: &QueueEntry) -> String {
    link(&truncate(&e.info.title, 80), &e.info.url)
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub async fn handle(lynx: Arc<Lynx>, http: Arc<Http>, interaction: Interaction) {
    if super::restrict::reject(&lynx, &http, &interaction).await {
        return;
    }
    match interaction {
        Interaction::Command(cmd) => command(&lynx, &http, &cmd).await,
        Interaction::Autocomplete(cmd) => autocomplete(&lynx, &http, &cmd).await,
        Interaction::Component(c) => component(&lynx, &http, &c).await,
        _ => {}
    }
}

/// Node bots answer every interaction with "add the main bot".
pub async fn handle_node(lynx: Arc<Lynx>, http: Arc<Http>, interaction: Interaction) {
    match interaction {
        Interaction::Command(cmd) => {
            let msg = cards::add_main(&lynx.main_invite(), &lynx.i18n);
            respond::respond(&http, cmd.id, &cmd.token, &msg.to_callback(callback::MESSAGE)).await;
        }
        Interaction::Autocomplete(cmd) => {
            respond::respond(&http, cmd.id, &cmd.token, &ui::autocomplete_callback(&[])).await;
        }
        _ => {}
    }
}

async fn command(lynx: &Lynx, http: &Http, cmd: &CommandInteraction) {
    let inv = Inv::new(
        lynx,
        http,
        cmd.id,
        &cmd.token,
        cmd.guild_id.map(|g| g.get()),
        cmd.channel_id.get(),
        cmd.member.as_deref(),
        &cmd.user,
    );
    if inv.guild_id == 0 {
        return inv.send(inv.err("guild_only", &[])).await;
    }
    if lynx.players.is_closing() {
        return inv.send(inv.err("restarting", &[])).await;
    }
    if cmd.data.name == "panel" {
        return inv.callback(bare_callback(callback::LAUNCH_ACTIVITY)).await;
    }
    if cmd.data.name == "ai" {
        if radio_ui::USE_MODAL && lynx.config.ai_enabled {
            return inv.callback(radio_ui::radio_modal(&lynx.i18n)).await;
        }
        let form = RadioForm {
            prompt: str_opt(cmd, "prompt"),
            mix: str_opt(cmd, "mix")
                .and_then(|m| crate::ai::RadioMix::parse(&m))
                .unwrap_or_default(),
            blend: bool_opt(cmd, "blend").unwrap_or(true),
        };
        return radio_ui::start(&radio_ctx(&inv), form).await;
    }
    let msg = match cmd.data.name.as_str() {
        "play" => play(&inv, &str_opt(cmd, "query").unwrap_or_default()).await,
        "pause" => pause(&inv).await,
        "resume" => resume(&inv).await,
        "skip" => skip(&inv, None).await,
        "stop" => stop(&inv, false).await,
        "queue" => queue(&inv, None, 0).await,
        "nowplaying" => now_playing(&inv).await,
        "remove" => remove(&inv, int_opt(cmd, "position")).await,
        "move" => move_track(&inv, int_opt(cmd, "from"), int_opt(cmd, "to")).await,
        "skipto" => skip_to(&inv, int_opt(cmd, "position")).await,
        "shuffle" => shuffle(&inv).await,
        "loop" => set_loop(&inv, &str_opt(cmd, "mode").unwrap_or_default()).await,
        "volume" => volume(&inv, int_opt(cmd, "value")).await,
        "lyrics" => lyrics(&inv, str_opt(cmd, "query")).await,
        other => {
            debug!(command = other, "unknown command");
            return;
        }
    };
    inv.send(msg).await;
}

fn str_opt(cmd: &CommandInteraction, name: &str) -> Option<String> {
    cmd.data
        .options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| match &o.value {
            CommandDataOptionValue::String(s) => Some(s.trim().to_owned()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
}

fn bool_opt(cmd: &CommandInteraction, name: &str) -> Option<bool> {
    cmd.data
        .options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| match &o.value {
            CommandDataOptionValue::Boolean(b) => Some(*b),
            _ => None,
        })
}

fn int_opt(cmd: &CommandInteraction, name: &str) -> i64 {
    cmd.data
        .options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| match &o.value {
            CommandDataOptionValue::Integer(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(0)
}

async fn autocomplete(lynx: &Lynx, http: &Http, cmd: &CommandInteraction) {
    let choices = match cmd.data.autocomplete() {
        Some(opt) if cmd.data.name == "play" => {
            let q = opt.value.trim();
            if q.chars().count() < 2 || q.starts_with("http://") || q.starts_with("https://") {
                Vec::new()
            } else {
                match tokio::time::timeout(AUTOCOMPLETE_TIMEOUT, lynx.sources.search(q, 8)).await {
                    Ok(Ok(tracks)) => tracks.iter().map(choice).collect(),
                    _ => Vec::new(),
                }
            }
        }
        _ => Vec::new(),
    };
    respond::respond(http, cmd.id, &cmd.token, &ui::autocomplete_callback(&choices)).await;
}

/// "Title - Artist (3:07)" -> track URL.
fn choice(t: &TrackInfo) -> (String, String) {
    let mut name = t.title.clone();
    if !t.artists.is_empty() {
        name.push_str(" - ");
        name.push_str(&t.artists.join(", "));
    }
    if let Some(d) = t.duration_ms.filter(|d| *d > 0) {
        name.push_str(&format!(" ({})", ui::format_duration(d)));
    }
    (truncate(&name, 100), t.url.clone())
}

// ---------------------------------------------------------------------------
// /play and bot assignment
// ---------------------------------------------------------------------------

fn source_error(inv: &Inv, e: &SourceError) -> Message {
    match e {
        SourceError::NotFound(_) => inv.err("no_results", &[]),
        SourceError::Unsupported(_) => inv.err("unsupported", &[]),
        SourceError::SpotifyDisabled => inv.err("spotify_disabled", &[]),
        SourceError::Timeout => inv.err("timeout", &[]),
        SourceError::Cancelled => inv.err("generic", &[]),
        SourceError::Unavailable(r) | SourceError::Http(r) | SourceError::Other(r) => {
            inv.err("source", &[("reason", r.trim_end_matches('.'))])
        }
    }
}

fn play_ctx(lynx: &Lynx) -> PlayCtx<'_> {
    PlayCtx {
        players: &lynx.players,
        bots: &lynx.bots,
        store: &lynx.store,
        sources: &lynx.sources,
    }
}

fn play_error(inv: &Inv, e: PlayError) -> Message {
    match e {
        PlayError::NoResults => inv.err("no_results", &[]),
        PlayError::Source(e) => source_error(inv, &e),
        PlayError::Timeout => inv.err("timeout", &[]),
        PlayError::CannotJoin { channel } => inv.err("cannot_connect", &[("channel", &format!("<#{channel}>"))]),
        PlayError::Busy {
            busy_channel,
            channel,
            invite,
        } => cards::all_busy(
            busy_channel,
            channel,
            invite.as_ref().map(|(n, u)| (*n, u.as_str())),
            &inv.lynx.i18n,
        ),
        PlayError::ShuttingDown => inv.err("restarting", &[]),
        PlayError::JoinFailed => inv.err("join_failed", &[]),
        PlayError::Denied(d) => inv.deny(d),
        PlayError::VoiceNotAllowed { allowed } => {
            inv.err("voice_channel", &[("channels", &super::restrict::mentions(&allowed))])
        }
        PlayError::QueueFull { max } => inv.err("queue_full", &[("max", &max.to_string())]),
        PlayError::Gone => gone(inv),
    }
}

/// Voice channel restriction (managers are never restricted).
async fn voice_allowed(inv: &Inv<'_>, vc: u64) -> Result<(), Message> {
    let manager = respond::is_manager(&inv.lynx.bots, inv.guild_id, inv.user_id, inv.permissions).await;
    play::check_voice(play_ctx(inv.lynx), inv.guild_id, vc, manager)
        .await
        .map_err(|e| play_error(inv, e))
}

/// Get (or create) the player for the user's voice channel.
async fn player_for_play(inv: &Inv<'_>, vc: u64, restore: Option<Restore>) -> Result<PlayerHandle, Message> {
    voice_allowed(inv, vc).await?;
    play::player_for(
        play_ctx(inv.lynx),
        inv.guild_id,
        vc,
        Some(inv.text_channel),
        inv.user_id,
        restore,
    )
    .await
    .map_err(|e| play_error(inv, e))
}

async fn play(inv: &Inv<'_>, query: &str) -> Message {
    let lynx = inv.lynx;
    let Some(vc) = inv.user_voice() else {
        return inv.err("not_in_voice", &[]);
    };
    if query.is_empty() {
        return inv.err("no_results", &[]);
    }
    if let Err(m) = voice_allowed(inv, vc).await {
        return m;
    }
    // Fail fast (before resolving) when no bot could play here.
    if let Err(e) = play::precheck(play_ctx(lynx), inv.guild_id, vc) {
        return play_error(inv, e);
    }
    inv.defer(false).await;

    let request = PlayRequest {
        guild_id: inv.guild_id,
        user_id: inv.user_id,
        voice_channel_id: vc,
        query,
        requester: inv.requester(),
        text_channel: Some(inv.text_channel),
    };
    let Played {
        player,
        entries,
        playlist_name,
        push: res,
    } = match play::play(play_ctx(lynx), request).await {
        Ok(p) => p,
        Err(e) => return play_error(inv, e),
    };
    let first = entries[0].clone();
    let is_list = playlist_name.is_some() || entries.len() > 1;
    let snap = player.snapshot();
    if !is_list {
        let eta = eta_ms(&snap, res.first_position);
        return cards::added_single(&card(&first), res.first_position, eta, &lynx.i18n);
    }
    let added = &entries[..res.added];
    let start = res.first_position.max(1);
    let preview = added
        .iter()
        .take(PLAYLIST_PREVIEW)
        .enumerate()
        .map(|(i, e)| (start + i, card(e)))
        .collect();
    let url = (query.starts_with("https://") || query.starts_with("http://")).then_some(query);
    cards::added_list(
        &cards::AddedList {
            name: playlist_name.as_deref(),
            url,
            origin: first.info.origin,
            count: res.added,
            total_ms: added.iter().filter_map(|e| e.info.duration_ms).sum(),
            thumbnail: first.info.thumbnail.as_deref(),
            preview,
            truncated: res.truncated,
            voice_channel_id: vc,
        },
        &lynx.i18n,
    )
}

/// Time until the track at a 1-based upcoming position plays.
fn eta_ms(snap: &PlayerSnapshot, position: usize) -> Option<u64> {
    if position == 0 {
        return None;
    }
    let current_left = snap
        .current
        .as_ref()
        .and_then(|c| c.info.duration_ms)
        .map(|d| d.saturating_sub(snap.position_now()))?;
    let before: Option<u64> = snap
        .upcoming
        .iter()
        .take(position - 1)
        .map(|e| e.info.duration_ms)
        .sum();
    Some(current_left + before?)
}

// ---------------------------------------------------------------------------
// Control commands
// ---------------------------------------------------------------------------

async fn pause(inv: &Inv<'_>) -> Message {
    let (p, _, _) = match inv.player_for(Action::Control).await {
        Ok(x) => x,
        Err(m) => return m,
    };
    match p.pause().await {
        Ok(true) => cards::confirm(inv.t("bot.reply.paused", &[])),
        Ok(false) => inv.err("already_paused", &[]),
        Err(_) => gone(inv),
    }
}

async fn resume(inv: &Inv<'_>) -> Message {
    let (p, _, _) = match inv.player_for(Action::Control).await {
        Ok(x) => x,
        Err(m) => return m,
    };
    match p.resume().await {
        Ok(true) => {
            let track = p.snapshot().current.as_ref().map(|e| track_link(e)).unwrap_or_default();
            cards::confirm(inv.t("bot.reply.resumed", &[("track", &track)]))
        }
        Ok(false) => inv.err("not_paused", &[]),
        Err(_) => gone(inv),
    }
}

/// `/skip` or the Skip / Vote buttons. `tag`: the button's track (must still be current).
async fn skip(inv: &Inv<'_>, button: Option<(PlayerHandle, &str)>) -> Message {
    let (p, settings, decision) = match button {
        Some((player, tag)) => {
            if player.snapshot().current.as_ref().map(|e| track_tag(&e.uid)).as_deref() != Some(tag) {
                return inv.err("outdated", &[]);
            }
            match inv.check(player, Action::Skip).await {
                Ok(x) => x,
                Err(m) => return m,
            }
        }
        None => match inv.player_for(Action::Skip).await {
            Ok(x) => x,
            Err(m) => return m,
        },
    };
    let force = decision == Decision::Allow;
    match p.skip(inv.user_id, force, settings.vote_skip).await {
        Ok(SkipOutcome::Skipped {
            skipped,
            next: Some(next),
        }) => cards::confirm(inv.t(
            "bot.reply.skipped",
            &[("track", &track_link(&skipped)), ("next", &track_link(&next))],
        )),
        Ok(SkipOutcome::Skipped { skipped, next: None }) => {
            cards::confirm(inv.t("bot.reply.skipped_last", &[("track", &track_link(&skipped))]))
        }
        Ok(SkipOutcome::Voted { current, required }) => {
            let snap = p.snapshot();
            match snap.current.as_ref() {
                Some(e) => cards::vote(
                    &e.info.title,
                    &e.info.url,
                    current,
                    required,
                    p.voice_channel_id(),
                    &track_tag(&e.uid),
                    &inv.lynx.i18n,
                ),
                None => gone(inv),
            }
        }
        Ok(SkipOutcome::AlreadyVoted { current, required }) => cards::confirm_private(
            Tone::Info,
            inv.t(
                "bot.reply.already_voted",
                &[("current", &current.to_string()), ("required", &required.to_string())],
            ),
        ),
        Ok(SkipOutcome::NothingPlaying) | Err(_) => gone(inv),
    }
}

async fn stop(inv: &Inv<'_>, announce: bool) -> Message {
    let (p, _, _) = match inv.player_for_local(Action::Control).await {
        Ok(x) => x,
        Err(m) => return m,
    };
    let vc = p.voice_channel_id();
    match p.stop(inv.user_id, announce).await {
        Ok(()) => cards::confirm(inv.t("bot.reply.stopped", &[("channel", &format!("<#{vc}>"))])),
        Err(_) => gone(inv),
    }
}

/// Queue card. `player`: from a button (already checked); `page`: 0-based.
async fn queue(inv: &Inv<'_>, player: Option<PlayerHandle>, page: u32) -> Message {
    let p = match player {
        Some(p) => p,
        None => match inv.player_for(Action::View).await {
            Ok((p, _, _)) => p,
            Err(m) => return m,
        },
    };
    queue_card(inv, &p.snapshot(), page, now_secs(), false)
}

fn queue_card(inv: &Inv, snap: &PlayerSnapshot, page: u32, issued: i64, expired: bool) -> Message {
    let pages = cards::page_count(snap.upcoming.len());
    let page = page.min(pages - 1);
    let start = page as usize * QUEUE_PAGE_SIZE;
    let entries = snap
        .upcoming
        .iter()
        .enumerate()
        .skip(start)
        .take(QUEUE_PAGE_SIZE)
        .map(|(i, e)| (i + 1, card(e)))
        .collect();
    let position = snap.position_now();
    let current_left = snap
        .current
        .as_ref()
        .and_then(|c| c.info.duration_ms)
        .map(|d| d.saturating_sub(position))
        .unwrap_or(0);
    let remaining = current_left + snap.upcoming.iter().filter_map(|e| e.info.duration_ms).sum::<u64>();
    cards::queue_page(
        &cards::QueuePage {
            voice_channel_id: snap.channel_id,
            current: snap.current.as_ref().map(|e| (card(e), position)),
            entries,
            page,
            total_upcoming: snap.upcoming.len(),
            remaining_ms: remaining,
            loop_mode: snap.loop_mode,
            issued,
            expired,
        },
        &inv.lynx.i18n,
    )
}

async fn now_playing(inv: &Inv<'_>) -> Message {
    let p = match inv.player_for(Action::View).await {
        Ok((p, _, _)) => p,
        Err(m) => return m,
    };
    let snap = p.snapshot();
    let Some(e) = snap.current.as_ref() else {
        return gone(inv);
    };
    let bot = inv.lynx.bots.bot(snap.bot_index);
    let node_name = bot.filter(|b| !b.is_main()).map(|b| b.name().to_owned());
    let tag = track_tag(&e.uid);
    let jam = snap
        .jam
        .as_ref()
        .map(|j| crate::player::messages::jam_line(j, p.key()));
    cards::now_playing_detail(
        &cards::NowPlayingDetail {
            track: card(e),
            voice_channel_id: snap.channel_id,
            node_name: node_name.as_deref(),
            jam: jam.as_ref(),
            position_ms: snap.position_now(),
            paused: snap.status == crate::player::state::Status::Paused,
            loop_mode: snap.loop_mode,
            volume: snap.volume,
            track_tag: &tag,
        },
        &inv.lynx.i18n,
    )
}

fn position_arg(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

async fn remove(inv: &Inv<'_>, position: i64) -> Message {
    let Some(vc) = inv.user_voice() else {
        return inv.err("not_in_voice", &[]);
    };
    let Some(player) = inv.lynx.players.get(inv.guild_id, vc) else {
        return gone(inv);
    };
    let pos = position_arg(position);
    let snap = player.snapshot();
    let max = snap.upcoming.len();
    let Some(target) = pos.checked_sub(1).and_then(|i| snap.upcoming.get(i)) else {
        return inv.err("invalid_position", &[("max", &max.max(1).to_string())]);
    };
    let own = target.requester.id == inv.user_id;
    let (p, _, _) = match inv.check(player, Action::Remove { own }).await {
        Ok(x) => x,
        Err(m) => return m,
    };
    // Remove by uid: the queue may have changed since the snapshot.
    match p.remove_uid(target.uid).await {
        Ok(Some(e)) => cards::confirm(inv.t("bot.reply.removed", &[("track", &track_link(&e))])),
        Ok(None) => inv.err("invalid_position", &[("max", &max.max(1).to_string())]),
        Err(_) => gone(inv),
    }
}

async fn move_track(inv: &Inv<'_>, from: i64, to: i64) -> Message {
    let (p, _, _) = match inv.player_for(Action::Control).await {
        Ok(x) => x,
        Err(m) => return m,
    };
    let (from, to) = (position_arg(from), position_arg(to));
    let max = p.snapshot().upcoming.len();
    match p.move_track(from, to).await {
        Ok(Some((e, to))) => cards::confirm(inv.t(
            "bot.reply.moved",
            &[
                ("track", &track_link(&e)),
                ("from", &from.to_string()),
                ("to", &to.to_string()),
            ],
        )),
        Ok(None) => inv.err("invalid_position", &[("max", &max.max(1).to_string())]),
        Err(_) => gone(inv),
    }
}

async fn skip_to(inv: &Inv<'_>, position: i64) -> Message {
    let (p, _, _) = match inv.player_for(Action::Control).await {
        Ok(x) => x,
        Err(m) => return m,
    };
    let max = p.snapshot().upcoming.len();
    match p.skip_to(position_arg(position)).await {
        Ok(Some((dropped, e))) => cards::confirm(inv.t(
            "bot.reply.skipped_to",
            &[("track", &track_link(&e)), ("count", &dropped.to_string())],
        )),
        Ok(None) => inv.err("invalid_position", &[("max", &max.max(1).to_string())]),
        Err(_) => gone(inv),
    }
}

async fn shuffle(inv: &Inv<'_>) -> Message {
    let (p, _, _) = match inv.player_for(Action::Control).await {
        Ok(x) => x,
        Err(m) => return m,
    };
    match p.shuffle().await {
        Ok(n) if n >= 2 => cards::confirm(inv.t("bot.reply.shuffled", &[("count", &n.to_string())])),
        Ok(_) => inv.err("nothing_to_shuffle", &[]),
        Err(_) => gone(inv),
    }
}

async fn set_loop(inv: &Inv<'_>, mode: &str) -> Message {
    let (p, _, _) = match inv.player_for(Action::Control).await {
        Ok(x) => x,
        Err(m) => return m,
    };
    let (mode, key) = match mode {
        "track" => (LoopMode::Track, "bot.reply.loop_track"),
        "queue" => (LoopMode::Queue, "bot.reply.loop_queue"),
        _ => (LoopMode::Off, "bot.reply.loop_off"),
    };
    match p.set_loop(mode).await {
        Ok(()) => cards::confirm(inv.t(key, &[])),
        Err(_) => gone(inv),
    }
}

async fn volume(inv: &Inv<'_>, value: i64) -> Message {
    let (p, _, _) = match inv.player_for_local(Action::Control).await {
        Ok(x) => x,
        Err(m) => return m,
    };
    let v = value.clamp(i64::from(crate::store::MIN_VOLUME), i64::from(crate::store::MAX_VOLUME)) as u16;
    match p.set_volume(v).await {
        Ok(prev) => cards::confirm(inv.t(
            "bot.reply.volume",
            &[("volume", &v.to_string()), ("previous", &prev.to_string())],
        )),
        Err(_) => gone(inv),
    }
}

async fn lyrics(inv: &Inv<'_>, query: Option<String>) -> Message {
    let lynx = inv.lynx;
    let (track, vc) = match query {
        Some(q) => {
            inv.defer(false).await;
            match tokio::time::timeout(Duration::from_secs(15), lynx.sources.search(&q, 1)).await {
                Ok(Ok(mut list)) if !list.is_empty() => (list.swap_remove(0), inv.user_voice()),
                Ok(_) => return inv.err("lyrics_not_found", &[]),
                Err(_) => return inv.err("timeout", &[]),
            }
        }
        None => {
            let p = match inv.player_for(Action::View).await {
                Ok((p, _, _)) => p,
                Err(_) => return inv.err("lyrics_nothing", &[]),
            };
            let snap = p.snapshot();
            let Some(e) = snap.current.as_ref() else {
                return inv.err("lyrics_nothing", &[]);
            };
            inv.defer(false).await;
            (e.info.clone(), Some(snap.channel_id))
        }
    };
    let found = match tokio::time::timeout(Duration::from_secs(15), lynx.sources.lyrics(&track)).await {
        Ok(Ok(Some(l))) => l,
        Ok(Ok(None)) | Ok(Err(_)) => return inv.err("lyrics_not_found", &[]),
        Err(_) => return inv.err("timeout", &[]),
    };
    let body = match (found.plain.as_deref(), found.synced.as_ref()) {
        (Some(p), _) if !p.trim().is_empty() => p.to_owned(),
        (_, Some(lines)) => lines.iter().map(|l| l.line.as_str()).collect::<Vec<_>>().join("\n"),
        _ => return inv.err("lyrics_not_found", &[]),
    };
    let c = TrackCard {
        title: &track.title,
        url: &track.url,
        artists: &track.artists,
        duration_ms: track.duration_ms,
        thumbnail: track.thumbnail.as_deref(),
        requester_id: inv.user_id,
        ai: false,
    };
    cards::lyrics(&c, &body, vc, &lynx.i18n)
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

async fn component(lynx: &Lynx, http: &Http, c: &ComponentInteraction) {
    let inv = Inv::new(
        lynx,
        http,
        c.id,
        &c.token,
        c.guild_id.map(|g| g.get()),
        c.channel_id.get(),
        c.member.as_ref(),
        &c.user,
    );
    if inv.guild_id == 0 {
        return;
    }
    let Some(id) = CustomId::parse(&c.data.custom_id) else {
        return;
    };
    match id {
        CustomId::NowPlaying {
            action: NpAction::Panel,
            ..
        } => inv.callback(bare_callback(callback::LAUNCH_ACTIVITY)).await,
        CustomId::NowPlaying {
            action,
            voice_channel_id,
            track,
        } => {
            let msg = np_button(&inv, action, voice_channel_id, &track).await;
            if let Some(msg) = msg {
                inv.send(msg).await;
            }
        }
        CustomId::QueuePage {
            voice_channel_id,
            page,
            issued,
        } => {
            let Some(p) = lynx.players.get(inv.guild_id, voice_channel_id) else {
                return inv.send(gone(&inv)).await;
            };
            let expired = now_secs() - issued > QUEUE_BUTTONS_TTL_SECS;
            let msg = queue_card(&inv, &p.snapshot(), page, issued, expired);
            inv.callback(msg.to_callback(callback::UPDATE_MESSAGE)).await;
            if expired {
                respond::followup(http, &c.token, &inv.err("expired", &[]).to_json()).await;
            }
        }
        CustomId::Replay { voice_channel_id } => {
            let msg = replay(&inv, voice_channel_id).await;
            inv.send(msg).await;
        }
        CustomId::StartRadio { .. } => radio_ui::start(&radio_ctx(&inv), RadioForm::default()).await,
        CustomId::StopRadio { voice_channel_id } => radio_ui::stop_button(&radio_ctx(&inv), voice_channel_id).await,
        CustomId::CancelRadio { voice_channel_id, job } => {
            radio_ui::cancel_button(&radio_ctx(&inv), voice_channel_id, &job).await
        }
    }
}

/// Now playing buttons. None = already answered (deferred update).
async fn np_button(inv: &Inv<'_>, action: NpAction, vc: u64, tag: &str) -> Option<Message> {
    let lynx = inv.lynx;
    let Some(player) = lynx.players.get(inv.guild_id, vc) else {
        return Some(gone(inv));
    };
    let current_tag = player.snapshot().current.as_ref().map(|e| track_tag(&e.uid));
    match action {
        NpAction::Pause | NpAction::Resume | NpAction::Stop => {
            let (p, _, _) = match inv.check_in(player, Action::Control, action != NpAction::Stop).await {
                Ok(x) => x,
                Err(m) => return Some(m),
            };
            // The player edits / replaces its own message; just acknowledge the click.
            inv.callback(serde_json::json!({ "type": 6 })).await;
            let _ = match action {
                NpAction::Pause => p.pause().await.map(|_| ()),
                NpAction::Resume => p.resume().await.map(|_| ()),
                _ => p.stop(inv.user_id, true).await,
            };
            None
        }
        NpAction::Skip => Some(skip(inv, Some((player, tag))).await),
        NpAction::Queue => {
            let (p, _, _) = match inv.check(player, Action::View).await {
                Ok(x) => x,
                Err(m) => return Some(m),
            };
            Some(queue(inv, Some(p), 0).await.ephemeral(true))
        }
        NpAction::Like | NpAction::Dislike => {
            if !lynx.config.ai_enabled {
                return Some(inv.err("generic", &[]));
            }
            if current_tag.as_deref() != Some(tag) {
                return Some(inv.err("outdated", &[]));
            }
            if let Err(m) = inv.check(player.clone(), Action::Feedback).await {
                return Some(m);
            }
            if !lynx.store.history_enabled(inv.user_id).await.unwrap_or(true) {
                return Some(cards::history_off(vc, &lynx.i18n));
            }
            let snap = player.snapshot();
            let e = snap.current.as_ref()?;
            let wanted = if action == NpAction::Like {
                Feedback::Like
            } else {
                Feedback::Dislike
            };
            let previous = lynx.store.get_feedback(inv.user_id, &e.info).await.ok().flatten();
            let value = (previous != Some(wanted)).then_some(wanted);
            if let Err(err) = lynx.store.set_feedback(inv.user_id, &e.info, value).await {
                warn!(error = %err, "could not save feedback");
                return Some(inv.err("generic", &[]));
            }
            // Feedback only: a Dislike never skips (it would bypass DJ mode and vote skip).
            let key = match value {
                Some(Feedback::Like) => "bot.reply.liked",
                Some(Feedback::Dislike) => "bot.reply.disliked",
                None => "bot.reply.feedback_cleared",
            };
            Some(cards::confirm_private(
                Tone::Ok,
                inv.t(key, &[("track", &track_link(e))]),
            ))
        }
        NpAction::Jam => Some(jam_button(inv, player, vc).await),
        NpAction::Panel => None,
    }
}

/// "Invite to jam": start a jam from this channel (or reuse the one it is in) and answer with its
/// Activity link: opening it launches the panel in the opener's own voice channel, where they can
/// join. A plain invite into this channel follows as a fallback when the bot may create one.
async fn jam_button(inv: &Inv<'_>, player: PlayerHandle, vc: u64) -> Message {
    let lynx = inv.lynx;
    let settings = inv.settings().await;
    let a = actor(
        &lynx.bots,
        &lynx.store,
        inv.guild_id,
        inv.user_id,
        inv.permissions,
        vc,
        false,
    )
    .await;
    if let Decision::Deny(d) = can_jam(&a, JamAction::Create, &settings) {
        return inv.deny(d);
    }
    let jams = lynx.players.jams();
    let jam = match jams.of(player.key()) {
        Some(j) => j,
        None => match player.jam_create().await {
            Ok(Ok(j)) => j,
            Ok(Err(CreateRefused::AlreadyInJam)) => match jams.of(player.key()) {
                Some(j) => j,
                None => return inv.err("generic", &[]),
            },
            Ok(Err(CreateRefused::TooMany)) | Err(_) => return inv.err("generic", &[]),
        },
    };
    let main = lynx.bots.main();
    let app = if main.application_id() != 0 {
        main.application_id()
    } else {
        lynx.config.discord_client_id.parse().unwrap_or(0)
    };
    let share = jam.view().share_link(&app.to_string());
    let can_invite = main
        .channel_permissions(inv.guild_id, vc)
        .is_none_or(|p| p.administrator() || p.create_instant_invite());
    let code = if can_invite {
        lynx.jam.get_or_create(&main.http, app, vc).await.ok()
    } else {
        None
    };
    cards::jam(vc, &share, code.as_deref(), &lynx.i18n)
}

/// "Play it again": restart the deleted queue in the user's voice channel.
async fn replay(inv: &Inv<'_>, original_vc: u64) -> Message {
    let lynx = inv.lynx;
    let Some(vc) = inv.user_voice() else {
        return inv.err("not_in_voice", &[]);
    };
    if lynx.players.get(inv.guild_id, vc).is_some() {
        return inv.err("generic", &[]);
    }
    let Some(restore) = lynx.players.take_ended(inv.guild_id, original_vc) else {
        return inv.err("nothing_to_replay", &[]);
    };
    let count = restore.tracks.len().saturating_sub(restore.index);
    let first = restore.tracks.get(restore.index).cloned();
    let total_ms = restore
        .tracks
        .iter()
        .skip(restore.index)
        .filter_map(|e| e.info.duration_ms)
        .sum();
    let preview: Vec<Entry> = restore
        .tracks
        .iter()
        .skip(restore.index)
        .take(PLAYLIST_PREVIEW)
        .cloned()
        .collect();
    match player_for_play(inv, vc, Some(restore)).await {
        Ok(p) => {
            let _ = p.set_text_channel(inv.text_channel).await;
            let Some(first) = first else {
                return inv.err("nothing_to_replay", &[]);
            };
            cards::added_list(
                &cards::AddedList {
                    name: None,
                    url: None,
                    origin: first.info.origin,
                    count,
                    total_ms,
                    thumbnail: first.info.thumbnail.as_deref(),
                    preview: preview.iter().enumerate().map(|(i, e)| (i, card(e))).collect(),
                    truncated: false,
                    voice_channel_id: vc,
                },
                &lynx.i18n,
            )
        }
        Err(m) => m,
    }
}
