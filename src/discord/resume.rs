//! Startup: wait for every bot's guilds, reconcile node commands, make stray bots leave voice and
//! resume the saved sessions (same bot when possible, current track from the start).

use std::{collections::HashSet, sync::Arc, time::Duration};

use serenity::all::GuildId;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use super::{
    commands,
    registry::{assign, Assignment, Candidate},
    Lynx,
};
use crate::{
    player::{QueueEntry, Restore, StartRequest},
    store::{SavedSession, SessionKey},
};

const SYNC_TIMEOUT: Duration = Duration::from_secs(90);
const PARALLEL_RESUMES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeDecision {
    /// Delete the saved session.
    Drop(&'static str),
    /// Reconnect with this bot. With nobody in the channel the player starts paused and the
    /// empty-channel timer runs.
    Resume { bot: usize },
}

/// Which bot resumes a session: the one that played it if it can, else any free bot.
pub fn decide(
    saved_bot: Option<usize>,
    channel_exists: bool,
    playable: bool,
    candidates: &[Candidate],
    channel: u64,
) -> ResumeDecision {
    if !channel_exists {
        return ResumeDecision::Drop("voice channel is gone");
    }
    if !playable {
        return ResumeDecision::Drop("nothing left to play");
    }
    if let Some(c) = saved_bot.and_then(|i| candidates.iter().find(|c| c.index == i)) {
        if c.ready && c.in_guild && c.can_play && !c.has_player && c.voice.is_none_or(|v| v == channel) {
            return ResumeDecision::Resume { bot: c.index };
        }
    }
    match assign(candidates, channel) {
        Assignment::Existing(bot) | Assignment::Free(bot) => ResumeDecision::Resume { bot },
        Assignment::CannotJoin | Assignment::Busy { .. } => ResumeDecision::Drop("no free bot"),
    }
}

pub async fn boot(lynx: Arc<Lynx>) {
    lynx.bots.wait_synced(SYNC_TIMEOUT).await;
    info!("all bots loaded their guilds");
    commands::reconcile_nodes(&lynx).await;

    let sessions = match lynx.store.list_sessions().await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "could not read saved sessions");
            Vec::new()
        }
    };
    leave_strays(&lynx, &sessions).await;
    // Let voice leaves settle before rejoining.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let sem = Arc::new(Semaphore::new(PARALLEL_RESUMES));
    let mut set = tokio::task::JoinSet::new();
    for saved in sessions {
        let lynx = lynx.clone();
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire().await;
            resume_one(&lynx, saved).await;
        });
    }
    while set.join_next().await.is_some() {}
    lynx.booted.store(true, std::sync::atomic::Ordering::Release);
    info!(players = lynx.players.len(), "startup finished");
}

/// A bot found in a voice channel without a saved session (for that bot) leaves it.
async fn leave_strays(lynx: &Lynx, sessions: &[SavedSession]) {
    let keep: HashSet<(SessionKey, u64)> = sessions
        .iter()
        .map(|s| (s.session.key(), s.session.bot_user_id))
        .collect();
    for (guild_id, index, channel) in lynx.bots.all_voice() {
        let Some(bot) = lynx.bots.bot(index) else { continue };
        let key = SessionKey {
            guild_id,
            voice_channel_id: channel,
        };
        if keep.contains(&(key, bot.user_id())) || lynx.players.get(guild_id, channel).is_some() {
            continue;
        }
        info!(
            guild = guild_id,
            channel,
            bot = index,
            "leaving a voice channel without a saved session"
        );
        let call = bot.songbird.get_or_insert(GuildId::new(guild_id));
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            let _ = call.lock().await.leave().await;
        })
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(5), bot.songbird.remove(GuildId::new(guild_id))).await;
        lynx.bots.set_voice(guild_id, index, None);
    }
}

async fn resume_one(lynx: &Lynx, saved: SavedSession) {
    let s = &saved.session;
    let (g, vc) = (s.guild_id, s.voice_channel_id);
    let index = s.current_index as usize;
    let candidates = lynx.bots.candidates(g, vc, |i| lynx.players.bot_has_player(g, i));
    let decision = decide(
        lynx.bots.index_of_user(s.bot_user_id),
        lynx.bots.channel_exists(g, vc),
        index < saved.tracks.len(),
        &candidates,
        vc,
    );
    let bot = match decision {
        ResumeDecision::Resume { bot } => bot,
        ResumeDecision::Drop(reason) => {
            info!(guild = g, channel = vc, reason, "dropping saved session");
            let _ = lynx.store.delete_session(s.key()).await;
            return;
        }
    };
    let settings = lynx.store.guild_settings(g).await.unwrap_or_default();
    let tracks = saved
        .tracks
        .iter()
        .map(|t| Arc::new(QueueEntry::from_session(t)))
        .collect();
    let req = StartRequest {
        guild_id: g,
        voice_channel_id: vc,
        bot_index: bot,
        text_channel_id: (s.text_channel_id != 0).then_some(s.text_channel_id),
        started_by: s.started_by,
        humans: lynx.bots.humans_in(g, vc),
        settings,
        restore: Some(Restore {
            tracks,
            index,
            loop_mode: s.loop_mode,
            volume: s.volume,
            after_restart: true,
        }),
    };
    let _guard = lynx.players.lock_guild(g).await;
    if lynx.players.get(g, vc).is_some() {
        // Someone already started playing there during startup; that player owns the session now.
        return;
    }
    match lynx.players.start(req).await {
        Ok(_) => info!(guild = g, channel = vc, bot, "session resumed"),
        Err(e) => {
            warn!(guild = g, channel = vc, error = %e, "could not resume a session");
            let _ = lynx.store.delete_session(s.key()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(index: usize, in_guild: bool, voice: Option<u64>, can_play: bool) -> Candidate {
        Candidate {
            index,
            ready: true,
            in_guild,
            voice,
            has_player: false,
            can_play,
        }
    }

    #[test]
    fn resume_decisions() {
        let all = [c(0, true, None, true), c(1, true, None, true)];
        // Same bot as before.
        assert_eq!(decide(Some(1), true, true, &all, 9), ResumeDecision::Resume { bot: 1 });
        // The bot is still sitting in that channel (it never left): same bot.
        let sitting = [c(0, true, None, true), c(1, true, Some(9), true)];
        assert_eq!(
            decide(Some(1), true, true, &sitting, 9),
            ResumeDecision::Resume { bot: 1 }
        );
        // The old bot left the guild: any free bot, main first.
        let left = [c(0, true, None, true), c(1, false, None, true)];
        assert_eq!(decide(Some(1), true, true, &left, 9), ResumeDecision::Resume { bot: 0 });
        // Unknown old bot (token removed): free bot.
        assert_eq!(decide(None, true, true, &all, 9), ResumeDecision::Resume { bot: 0 });
        // The old bot lost its permissions there: another one.
        let no_perm = [c(0, true, None, true), c(1, true, None, false)];
        assert_eq!(
            decide(Some(1), true, true, &no_perm, 9),
            ResumeDecision::Resume { bot: 0 }
        );
        // Old bot busy in another channel resumed first: another free one.
        let busy = [c(0, true, None, true), c(1, true, Some(3), true)];
        assert_eq!(decide(Some(1), true, true, &busy, 9), ResumeDecision::Resume { bot: 0 });
        // Nothing free.
        let none = [c(0, true, Some(3), true), c(1, false, None, true)];
        assert!(matches!(decide(Some(0), true, true, &none, 9), ResumeDecision::Drop(_)));
        // Channel deleted or queue over.
        assert!(matches!(decide(Some(0), false, true, &all, 9), ResumeDecision::Drop(_)));
        assert!(matches!(decide(Some(0), true, false, &all, 9), ResumeDecision::Drop(_)));
    }
}
