mod api;
mod config;
mod discord;
mod i18n;
mod perms;
mod player;
mod procs;
mod sources;
mod store;

use std::sync::Arc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{config::Config, i18n::I18n, procs::ProcessRegistry, store::Store};

/// Shared handles every subsystem can clone.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Store,
    pub i18n: Arc<I18n>,
    pub procs: ProcessRegistry,
    pub shutdown: CancellationToken,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LOG_LEVEL")
                .unwrap_or_else(|_| "info,serenity=warn,songbird=warn,sqlx=warn".into()),
        )
        .init();

    let config = Arc::new(Config::from_env()?);
    tokio::fs::create_dir_all(&config.data_dir).await?;
    // Temporary stream fragments never survive a restart.
    let _ = tokio::fs::remove_dir_all(config.streams_dir()).await;
    tokio::fs::create_dir_all(config.streams_dir()).await?;

    let shutdown = CancellationToken::new();
    let state = AppState {
        store: Store::open(&config.database_path()).await?,
        i18n: Arc::new(I18n::load(&config.bot_locale)?),
        procs: ProcessRegistry::new(),
        config: config.clone(),
        shutdown: shutdown.clone(),
    };

    let api = tokio::spawn(api::serve(state.clone()));
    let bots = tokio::spawn(discord::run(state.clone()));

    wait_for_signal().await;
    info!("shutdown requested");
    shutdown.cancel();
    state.procs.kill_all().await;

    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let _ = bots.await;
        let _ = api.await;
    })
    .await;
    state.store.close().await;
    info!("bye");
    Ok(())
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
