mod ai;
mod api;
mod config;
mod discord;
mod i18n;
mod limits;
mod perms;
mod player;
mod procs;
mod sources;
mod store;
mod text;

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    ai::AiEngine,
    config::Config,
    discord::{about::YtdlpVersion, jam::JamInvites, registry::BotRegistry, Bots, Lynx},
    i18n::I18n,
    player::{PlayerDeps, PlayerManager},
    procs::ProcessRegistry,
    sources::Sources,
    store::{SessionWriter, Store},
};

/// How often orphaned child processes are looked for, and the oldest a child may get.
const PROC_SWEEP_EVERY: Duration = Duration::from_secs(60);
const PROC_MAX_AGE: Duration = Duration::from_secs(7 * 60 * 60);
/// How often the stream temp dirs are checked against `STREAM_CACHE_MAX_MB`.
const STREAM_LIMIT_EVERY: Duration = Duration::from_secs(5 * 60);

/// Shared handles every subsystem can clone (the panel API uses `players`, `bots` and `sources`).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Store,
    pub i18n: Arc<I18n>,
    pub procs: ProcessRegistry,
    pub sources: Sources,
    pub bots: Arc<BotRegistry>,
    pub players: PlayerManager,
    pub ai: AiEngine,
    /// Cached yt-dlp version (`/about` and the health endpoint).
    pub ytdlp_version: Arc<YtdlpVersion>,
    /// Per-user rate limits shared by the panel and the Discord commands.
    pub limits: Arc<limits::UserLimits>,
    pub shutdown: CancellationToken,
}

#[tokio::main]
async fn main() -> Result<()> {
    discord::about::mark_start();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LOG_LEVEL")
                .unwrap_or_else(|_| "info,serenity=warn,songbird=warn,sqlx=warn".into()),
        )
        .init();

    let config = Arc::new(Config::from_env()?);
    tokio::fs::create_dir_all(&config.appdata_dir).await?;
    tokio::fs::create_dir_all(&config.data_dir).await?;
    // Temporary stream fragments never survive a restart.
    let _ = tokio::fs::remove_dir_all(config.streams_dir()).await;
    tokio::fs::create_dir_all(config.streams_dir()).await?;

    let shutdown = CancellationToken::new();
    let store = Store::open(&config.database_path()).await?;
    let i18n = Arc::new(I18n::load(&config.bot_locale)?);
    let procs = ProcessRegistry::new();
    let sources = Sources::new(&config, store.clone(), procs.clone())?;
    let (sessions, session_task) = SessionWriter::spawn(store.clone());

    // Background housekeeping.
    let maintenance = store::spawn_maintenance(
        store.clone(),
        Duration::from_secs(u64::from(config.cache_ttl_days.max(1)) * 24 * 60 * 60),
        shutdown.clone(),
    );
    tokio::spawn(sources.update_task(shutdown.clone()));
    let ai = AiEngine::new(&config, store.clone(), sources.clone());
    let ai_tasks = ai.spawn_background(shutdown.clone());

    let bots = Bots::build(&config).await?;
    let players = PlayerManager::new(PlayerDeps {
        config: config.clone(),
        i18n: i18n.clone(),
        store: store.clone(),
        sources: sources.clone(),
        procs: procs.clone(),
        sessions: sessions.clone(),
        bots: bots.registry.clone(),
        ai: ai.clone(),
        // Fetches stream URLs (yt-dlp output for user links): public addresses only, redirects re-checked.
        http: sources::netguard::guarded(reqwest::Client::builder().connect_timeout(Duration::from_secs(10))).build()?,
    });
    {
        let players = players.clone();
        tokio::spawn(procs.sweep_task(
            PROC_SWEEP_EVERY,
            PROC_MAX_AGE,
            move |owner| players.owner_alive(owner),
            shutdown.clone(),
        ));
    }
    tokio::spawn(stream_limit_task(sources.clone(), players.clone(), shutdown.clone()));

    let ytdlp_version = Arc::new(YtdlpVersion::default());
    let limits = Arc::new(limits::UserLimits::default());
    let state = AppState {
        config: config.clone(),
        store: store.clone(),
        i18n: i18n.clone(),
        procs: procs.clone(),
        sources: sources.clone(),
        bots: bots.registry.clone(),
        players: players.clone(),
        ai: ai.clone(),
        ytdlp_version: ytdlp_version.clone(),
        limits: limits.clone(),
        shutdown: shutdown.clone(),
    };
    let lynx = Arc::new(Lynx {
        limits: limits.clone(),
        config: config.clone(),
        i18n,
        store: store.clone(),
        sources,
        bots: bots.registry.clone(),
        players: players.clone(),
        jam: JamInvites::new(),
        ytdlp_version,
        booted: Default::default(),
    });
    lynx.ytdlp_version.refresh_if_stale(&lynx.sources);
    let gateway_stop = bots.stop.clone();
    let gateways = bots.start(lynx);
    let api = {
        let state = state.clone();
        tokio::spawn(async move {
            // Without the HTTP server the panel and health checks are dead: log why and exit so the
            // container restarts instead of staying half alive.
            if let Err(e) = api::serve(state).await {
                tracing::error!(error = %e, "http server failed");
                std::process::exit(1);
            }
        })
    };

    wait_for_signal().await;
    info!("shutdown requested");
    // 1. Stop accepting work and save every session as it is; leave voice.
    players.shutdown().await;
    // 2. Write the sessions now.
    match sessions.flush_now().await {
        Ok(n) => info!(sessions = n, "sessions saved"),
        Err(e) => warn!(error = %e, "could not flush sessions"),
    }
    // 3. Disconnect the gateways and stop background tasks and the API.
    gateway_stop.cancel();
    shutdown.cancel();
    // 4. No child process survives.
    procs.kill_all().await;

    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        let _ = gateways.await;
        let _ = api.await;
        let _ = maintenance.await;
        for t in ai_tasks {
            let _ = t.await;
        }
    })
    .await;
    drop(state);
    drop(players);
    drop(sessions);
    let _ = tokio::time::timeout(Duration::from_secs(5), session_task).await;
    store.close().await;
    info!("bye");
    Ok(())
}

/// Keep the stream temp dirs under `STREAM_CACHE_MAX_MB`, never touching live tracks.
async fn stream_limit_task(sources: Sources, players: PlayerManager, stop: CancellationToken) {
    let mut tick = tokio::time::interval(STREAM_LIMIT_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => {}
        }
        let p = players.clone();
        let freed = sources.stream_dirs().enforce_limit(move |g, c, t| p.protects_dir(g, c, t)).await;
        if freed > 0 {
            info!(freed_bytes = freed, "stream cache over its limit, removed old fragments");
        }
    }
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
