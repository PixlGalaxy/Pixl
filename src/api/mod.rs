//! HTTP server: Activity API, WebSocket events, CDN proxy and the built panel (static files).

use std::{net::SocketAddr, path::PathBuf};

use anyhow::Result;
use axum::{routing::get, Router};
use tower_http::services::{ServeDir, ServeFile};
use tracing::info;

use crate::AppState;

pub async fn serve(state: AppState) -> Result<()> {
    let web_dir = std::env::var("WEB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./web/dist"));
    let static_files = ServeDir::new(&web_dir).fallback(ServeFile::new(web_dir.join("index.html")));

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .fallback_service(static_files)
        .with_state(state.clone());

    let addr: SocketAddr = format!("{}:{}", state.config.http_host, state.config.http_port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "http listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { state.shutdown.cancelled().await })
        .await?;
    Ok(())
}
