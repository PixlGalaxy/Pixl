//! The `/play` flow shared by the slash command and the panel API (`POST /api/queue/add`):
//! resolve the input, pick a bot for the user's voice channel (or reuse the one there), start the
//! player and enqueue. Callers turn [`PlayError`] into their own message or error code.
//!
//! Lists (playlists, albums, artist pages) are read through the shared list cache and saved to the
//! requester's saved playlists (Playlists tab of the panel).

use std::{sync::Arc, time::Duration};

use tracing::{debug, warn};

use super::registry::{assign, Assignment, BotRegistry};
use crate::{
    perms::{can, channel_allowed, Action, Actor, Decision, Deny},
    player::{
        Entry, PlayerHandle, PlayerManager, PushError, PushResult, QueueEntry, Requester, Restore, StartError,
        StartRequest,
    },
    procs::ProcOwner,
    sources::{ListRef, Resolved, SourceError, Sources, DEFAULT_LIST_LIMIT},
    store::{GuildSettings, NewPlaylist, SaveOutcome, Store},
};

pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(120);

/// What the flow needs from the app.
#[derive(Clone, Copy)]
pub struct PlayCtx<'a> {
    pub players: &'a PlayerManager,
    pub bots: &'a BotRegistry,
    pub store: &'a Store,
    pub sources: &'a Sources,
    /// `MAX_STORED_PLAYLISTS`: saved lists per user (0: lists are not saved).
    pub max_playlists: usize,
}

#[derive(Debug)]
pub enum PlayError {
    /// Empty input or nothing found.
    NoResults,
    Source(SourceError),
    /// Resolving took longer than [`RESOLVE_TIMEOUT`].
    Timeout,
    /// A bot is free but cannot connect / speak in the channel.
    CannotJoin {
        channel: u64,
    },
    /// Every Lynx bot of the guild is busy elsewhere. `invite`: next node to invite (number, URL).
    Busy {
        busy_channel: Option<u64>,
        channel: u64,
        invite: Option<(usize, String)>,
    },
    ShuttingDown,
    JoinFailed,
    Denied(Deny),
    /// A manager limited Lynx to other voice channels (the allowed ones).
    VoiceNotAllowed {
        allowed: Vec<u64>,
    },
    QueueFull {
        max: usize,
    },
    /// The player ended while enqueueing.
    Gone,
}

/// One `/play` request from a user who is in `voice_channel_id`.
pub struct PlayRequest<'a> {
    pub guild_id: u64,
    pub user_id: u64,
    pub voice_channel_id: u64,
    pub query: &'a str,
    pub requester: Requester,
    /// Text channel for the player's messages. `None` keeps the player's current one; a new player
    /// then posts in the voice channel's own chat.
    pub text_channel: Option<u64>,
}

pub struct Played {
    pub player: PlayerHandle,
    /// Every resolved entry (the first `push.added` were queued).
    pub entries: Vec<Entry>,
    pub playlist_name: Option<String>,
    pub push: PushResult,
}

fn busy(ctx: PlayCtx<'_>, guild_id: u64, channel: u64, busy_channel: Option<u64>) -> PlayError {
    PlayError::Busy {
        busy_channel,
        channel,
        invite: ctx.bots.next_node_invite(guild_id),
    }
}

/// Voice channel restriction: managers play anywhere, others only in the allowed channels.
pub async fn check_voice(ctx: PlayCtx<'_>, guild_id: u64, vc: u64, is_manager: bool) -> Result<(), PlayError> {
    let rules = ctx.store.channel_rules(guild_id).await.unwrap_or_default();
    if channel_allowed(&rules.voice_channels, vc, is_manager) {
        Ok(())
    } else {
        Err(PlayError::VoiceNotAllowed {
            allowed: rules.voice_channels,
        })
    }
}

/// Fail fast (before resolving) when no bot could play in `vc`.
pub fn precheck(ctx: PlayCtx<'_>, guild_id: u64, vc: u64) -> Result<(), PlayError> {
    if ctx.players.get(guild_id, vc).is_some() {
        return Ok(());
    }
    let candidates = ctx
        .bots
        .candidates(guild_id, vc, |i| ctx.players.bot_has_player(guild_id, i));
    match assign(&candidates, vc) {
        Assignment::CannotJoin => Err(PlayError::CannotJoin { channel: vc }),
        Assignment::Busy { busy_channel } => Err(busy(ctx, guild_id, vc, busy_channel)),
        Assignment::Existing(_) | Assignment::Free(_) => Ok(()),
    }
}

/// Get (or create) the player for `vc`. Holds the guild lock while assigning a bot.
pub async fn player_for(
    ctx: PlayCtx<'_>,
    guild_id: u64,
    vc: u64,
    text_channel: Option<u64>,
    started_by: u64,
    restore: Option<Restore>,
) -> Result<PlayerHandle, PlayError> {
    let _guard = ctx.players.lock_guild(guild_id).await;
    if let Some(p) = ctx.players.get(guild_id, vc) {
        return Ok(p);
    }
    let candidates = ctx
        .bots
        .candidates(guild_id, vc, |i| ctx.players.bot_has_player(guild_id, i));
    let bot_index = match assign(&candidates, vc) {
        Assignment::Existing(i) | Assignment::Free(i) => i,
        Assignment::CannotJoin => return Err(PlayError::CannotJoin { channel: vc }),
        Assignment::Busy { busy_channel } => return Err(busy(ctx, guild_id, vc, busy_channel)),
    };
    let req = StartRequest {
        guild_id,
        voice_channel_id: vc,
        bot_index,
        text_channel_id: text_channel.or(Some(vc)),
        started_by,
        humans: ctx.bots.humans_in(guild_id, vc),
        settings: ctx.store.guild_settings(guild_id).await.unwrap_or_default(),
        restore,
    };
    match ctx.players.start(req).await {
        Ok(p) => Ok(p),
        Err(StartError::ShuttingDown) => Err(PlayError::ShuttingDown),
        Err(_) => Err(PlayError::JoinFailed),
    }
}

fn queue_limit(settings: &GuildSettings) -> usize {
    (settings.max_queue_length as usize).min(DEFAULT_LIST_LIMIT)
}

/// Resolve, start (or reuse) the player and enqueue. Does not run [`precheck`]: callers that want to
/// fail before a slow resolve call it first. A list is saved to the requester's playlists.
pub async fn play(ctx: PlayCtx<'_>, req: PlayRequest<'_>) -> Result<Played, PlayError> {
    let query = req.query.trim();
    if query.is_empty() {
        return Err(PlayError::NoResults);
    }
    let settings = ctx.store.guild_settings(req.guild_id).await.unwrap_or_default();
    let owner = ProcOwner::new(req.guild_id, req.voice_channel_id, None);
    let resolving = ctx.sources.resolve_for_play(query, queue_limit(&settings), &owner);
    let (resolved, list) = match tokio::time::timeout(RESOLVE_TIMEOUT, resolving).await {
        Ok(Ok((r, list))) if !r.tracks.is_empty() => (r, list),
        Ok(Ok(_)) | Ok(Err(SourceError::NotFound(_))) => return Err(PlayError::NoResults),
        Ok(Err(e)) => return Err(PlayError::Source(e)),
        Err(_) => return Err(PlayError::Timeout),
    };
    let saved = list.map(|l| (l, resolved.playlist_name.clone(), resolved.image.clone()));
    let played = enqueue(ctx, &req, &settings, resolved).await?;
    if let Some((list, name, image)) = saved {
        remember_list(ctx, req.user_id, &list, name.as_deref(), image.as_deref()).await;
    }
    Ok(played)
}

/// [`play`] with tracks that are already resolved (a saved playlist, or one of its songs). Same
/// player assignment and permission checks; nothing is saved.
pub async fn play_tracks(ctx: PlayCtx<'_>, req: PlayRequest<'_>, mut resolved: Resolved) -> Result<Played, PlayError> {
    let settings = ctx.store.guild_settings(req.guild_id).await.unwrap_or_default();
    resolved.tracks.truncate(queue_limit(&settings));
    if resolved.tracks.is_empty() {
        return Err(PlayError::NoResults);
    }
    enqueue(ctx, &req, &settings, resolved).await
}

/// Save a list the user just queued (or move it to the top of their saved playlists). Users who
/// turned their listening history off are not saved to. Failures only log.
async fn remember_list(ctx: PlayCtx<'_>, user_id: u64, list: &ListRef, name: Option<&str>, image: Option<&str>) {
    if ctx.max_playlists == 0 || !ctx.store.history_enabled(user_id).await.unwrap_or(false) {
        return;
    }
    let p = NewPlaylist {
        url: &list.url,
        platform: list.platform.as_str(),
        name: name.unwrap_or_default(),
        image_url: image,
    };
    match ctx.store.save_playlist(user_id, &p, ctx.max_playlists).await {
        Ok(SaveOutcome::Saved { evicted, .. }) => debug!(user = user_id, url = %list.url, evicted, "playlist saved"),
        Ok(SaveOutcome::Full) => debug!(user = user_id, url = %list.url, "playlist not saved: every slot is pinned"),
        Err(e) => warn!(error = %e, user = user_id, "could not save playlist"),
    }
}

async fn enqueue(
    ctx: PlayCtx<'_>,
    req: &PlayRequest<'_>,
    settings: &GuildSettings,
    resolved: Resolved,
) -> Result<Played, PlayError> {
    let player = player_for(
        ctx,
        req.guild_id,
        req.voice_channel_id,
        req.text_channel,
        req.user_id,
        None,
    )
    .await?;
    // Adding only depends on being in the player's channel (re-read: the user may have moved while
    // the input resolved).
    let actor = Actor {
        in_channel: ctx.bots.user_voice_channel(req.guild_id, req.user_id) == Some(player.voice_channel_id()),
        ..Actor::default()
    };
    if let Decision::Deny(d) = can(&actor, Action::Add, settings) {
        return Err(PlayError::Denied(d));
    }
    let entries: Vec<Entry> = resolved
        .tracks
        .iter()
        .map(|t| Arc::new(QueueEntry::new(t.clone(), req.requester.clone())))
        .collect();
    let push = match player
        .enqueue(entries.clone(), settings.max_queue_length as usize, req.text_channel)
        .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(PushError::Full { max })) => return Err(PlayError::QueueFull { max }),
        Err(_) => return Err(PlayError::Gone),
    };
    Ok(Played {
        player,
        entries,
        playlist_name: resolved.playlist_name,
        push,
    })
}
