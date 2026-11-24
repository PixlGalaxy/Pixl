//! Status snapshot for the dashboard, computed every few seconds on the BOT's runtime (where the
//! gateways, players and caches live) and published through a `watch` channel. Admin handlers only
//! read the latest value, so a busy admin panel never makes the bot compute anything extra.

use std::{collections::HashSet, sync::Arc, time::Duration};

use serde::Serialize;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{
    api::health::{self, HealthReport},
    discord::about,
    player::state::Status as PlayerStatus,
    store::now_secs,
    AppState,
};

const EVERY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    /// Servers with at least one Lynx bot.
    pub servers: usize,
    pub players_active: usize,
    pub players_playing: usize,
    /// People in voice channels with a Lynx player who can hear it (deafened users left out).
    pub listeners: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub health: HealthReport,
    /// "Main", "Voice A", "Voice B"... per bot, in `health.bots` order.
    pub labels: Vec<String>,
    pub totals: Totals,
    pub memory_bytes: Option<u64>,
    /// Epoch ms when it was computed.
    pub at: i64,
}

/// "Main" for the first bot, then "Voice A", "Voice B", ... ("Voice 27" past Z).
pub fn label(index: usize) -> String {
    match index {
        0 => "Main".into(),
        i if i <= 26 => format!("Voice {}", char::from(b'A' + (i - 1) as u8)),
        i => format!("Voice {i}"),
    }
}

pub async fn compute(app: &AppState) -> Snapshot {
    let report = health::compute(app, &health::LIMITS).await;
    let players = app.players.all();
    let servers: HashSet<u64> = (0..app.bots.bots().len()).flat_map(|i| app.bots.guilds_of(i)).collect();
    let listeners = players
        .iter()
        .map(|p| app.bots.listeners_in(p.guild_id, p.voice_channel_id()).len())
        .sum();
    Snapshot {
        labels: report.bots.iter().map(|b| label(b.index)).collect(),
        totals: Totals {
            servers: servers.len(),
            players_active: players.len(),
            players_playing: players
                .iter()
                .filter(|p| p.snapshot().status == PlayerStatus::Playing)
                .count(),
            listeners,
        },
        health: report,
        memory_bytes: about::memory_bytes(),
        at: now_secs() * 1000,
    }
}

/// Recompute every [`EVERY`] on the current (bot) runtime until `stop`.
pub fn spawn(app: AppState, stop: CancellationToken) -> watch::Receiver<Option<Arc<Snapshot>>> {
    let (tx, rx) = watch::channel(None);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tick.tick() => {}
            }
            let s = compute(&app).await;
            if tx.send(Some(Arc::new(s))).is_err() {
                return;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    #[test]
    fn labels() {
        assert_eq!(super::label(0), "Main");
        assert_eq!(super::label(1), "Voice A");
        assert_eq!(super::label(3), "Voice C");
        assert_eq!(super::label(27), "Voice 27");
    }
}
