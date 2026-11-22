//! AI engine: genre tagging, taste vectors, similar users and the `/ai` radio.
//!
//! [`AiEngine`] is the only entry point. It is cheap to clone. The player calls
//! [`AiEngine::start_radio`] / [`AiEngine::refill`] and reads phases and picks from the returned
//! [`RadioJob`]; it never talks to Last.fm, MusicBrainz or Ollama directly.

#![allow(dead_code)]

pub mod candidates;
pub mod lastfm;
pub mod limiter;
pub mod musicbrainz;
pub mod ollama;
mod queries;
mod radio;
pub mod tagger;
pub mod taste;
pub mod taxonomy;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::{
    sync::{oneshot, watch, Notify, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    config::Config,
    sources::{Sources, TrackInfo},
    store::{track_key, Store},
};

/// Radio jobs that may run their DB / network phases at once (Ollama itself is limited to 1).
const RADIO_SLOTS: usize = 8;
const TASTE_INTERVAL: Duration = Duration::from_secs(300);

/// A wait or request was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// Errors of the HTTP clients (Last.fm, MusicBrainz, Ollama).
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("cancelled")]
    Cancelled,
    /// Network, timeout or HTTP status problem (transient).
    #[error("{0}")]
    Http(String),
    /// The service answered with an error or something unexpected.
    #[error("{0}")]
    Api(String),
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Self::Http("timed out".into())
        } else if e.is_decode() {
            Self::Api("unexpected response".into())
        } else {
            // Never include URLs: they carry the API key.
            Self::Http(e.without_url().to_string())
        }
    }
}

/// Phase of a radio job, as shown in the panel (`AiPhase` in the API contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiPhase {
    Queued,
    LoadingModel,
    ReadingTaste,
    FindingSongs,
    Thinking,
    Resolving,
    Ready,
    Failed,
    Cancelled,
}

impl AiPhase {
    pub fn is_final(self) -> bool {
        matches!(self, Self::Ready | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonKey {
    BecauseYouLike,
    SimilarListeners,
    YourFavorite,
    MatchesPrompt,
}

/// "Why this song": `{ key, params }` (`Track.reason` in the API contract; the panel translates).
/// Params: `because_you_like` -> `name`, `matches_prompt` -> `prompt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    pub key: ReasonKey,
    pub params: BTreeMap<String, String>,
}

impl Reason {
    pub fn new(key: ReasonKey) -> Self {
        Self {
            key,
            params: BTreeMap::new(),
        }
    }

    pub fn because_you_like(name: &str) -> Self {
        Self {
            key: ReasonKey::BecauseYouLike,
            params: [("name".to_owned(), name.to_owned())].into(),
        }
    }

    pub fn matches_prompt(prompt: &str) -> Self {
        let p: String = prompt.chars().take(100).collect();
        Self {
            key: ReasonKey::MatchesPrompt,
            params: [("prompt".to_owned(), p)].into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RadioRequest {
    pub guild_id: u64,
    pub channel_id: u64,
    /// Who started the radio. Must have history enabled.
    pub owner_id: u64,
    /// Everyone else in the voice channel (bots excluded). Users with history off are ignored for
    /// taste blending, but their dislikes (if any were stored) still filter.
    pub listener_ids: Vec<u64>,
    pub prompt: Option<String>,
    /// Catalog keys (`store::track_key`) played or queued recently in this session: never picked.
    pub recent_keys: Vec<String>,
    /// How many picks to return (clamped to 1..=50; ~20 for a start, ~10 for a refill).
    pub batch_size: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RadioPick {
    pub track: TrackInfo,
    pub reason: Reason,
    /// Known genres (may be empty), for display.
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RadioError {
    /// `AI_ENABLED` is off.
    #[error("the AI radio is disabled on this bot")]
    Disabled,
    /// The owner turned their history off (API error code `history_disabled`).
    #[error("history is disabled for this user")]
    HistoryDisabled,
    #[error("no songs found")]
    NoCandidates,
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Internal(String),
}

impl From<anyhow::Error> for RadioError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<Cancelled> for RadioError {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}

/// A running radio job. Dropping it cancels the job.
pub struct RadioJob {
    pub id: Uuid,
    /// Latest phase; ends on `ready`, `failed` or `cancelled`.
    pub phases: watch::Receiver<AiPhase>,
    first: Option<oneshot::Receiver<RadioPick>>,
    cancel: CancellationToken,
    handle: Option<JoinHandle<Result<Vec<RadioPick>, RadioError>>>,
}

impl RadioJob {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn phase(&self) -> AiPhase {
        *self.phases.borrow()
    }

    /// The algorithm's first pick, sent before Ollama runs so playback can start right away.
    /// None for refills, or when the job ended without one. It is not repeated in `result`.
    pub async fn first_pick(&mut self) -> Option<RadioPick> {
        self.first.take()?.await.ok()
    }

    /// Wait for the remaining picks.
    pub async fn result(mut self) -> Result<Vec<RadioPick>, RadioError> {
        let handle = self.handle.take().expect("result is only taken once");
        match handle.await {
            Ok(r) => r,
            Err(e) if e.is_cancelled() => Err(RadioError::Cancelled),
            Err(e) => Err(RadioError::Internal(e.to_string())),
        }
    }
}

impl Drop for RadioJob {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.cancel.cancel();
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AiSettings {
    pub enabled: bool,
    pub cache_ttl: Duration,
}

pub(crate) struct EngineInner {
    pub settings: AiSettings,
    pub store: Store,
    pub sources: Sources,
    pub lastfm: Option<lastfm::LastFm>,
    pub musicbrainz: Option<musicbrainz::MusicBrainz>,
    pub ollama: Option<ollama::Ollama>,
    pub tagger_wake: Arc<Notify>,
    pub radio_slots: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct AiEngine {
    inner: Arc<EngineInner>,
}

impl AiEngine {
    pub fn new(config: &Config, store: Store, sources: Sources) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .user_agent(crate::sources::USER_AGENT)
            .build()
            .unwrap_or_default();
        let ttl = Duration::from_secs(u64::from(config.cache_ttl_days.max(1)) * 86_400);
        let lastfm = config
            .lastfm_api_key
            .clone()
            .map(|k| lastfm::LastFm::new(http.clone(), k, store.clone(), ttl));
        let musicbrainz = musicbrainz::MusicBrainz::new(
            http.clone(),
            config.musicbrainz_user_agent.as_deref(),
            store.clone(),
            ttl,
        );
        let ollama = match (config.ai_enabled, &config.ollama_url, &config.ollama_model) {
            (true, Some(url), Some(model)) => Some(ollama::Ollama::new(
                http.clone(),
                url,
                model,
                &config.ollama_keep_alive,
                Duration::from_secs(config.ollama_timeout_secs),
            )),
            _ => None,
        };
        Self {
            inner: Arc::new(EngineInner {
                settings: AiSettings {
                    enabled: config.ai_enabled,
                    cache_ttl: ttl,
                },
                store,
                sources,
                lastfm,
                musicbrainz,
                ollama,
                tagger_wake: Arc::new(Notify::new()),
                radio_slots: Arc::new(Semaphore::new(RADIO_SLOTS)),
            }),
        }
    }

    /// `AI_ENABLED`: radio, Like/Dislike and history.
    pub fn enabled(&self) -> bool {
        self.inner.settings.enabled
    }

    pub fn ollama_configured(&self) -> bool {
        self.inner.ollama.is_some()
    }

    /// Genre tagging runs whenever a tag source is configured (now-playing genres work without
    /// AI_ENABLED); the taste job only with AI_ENABLED. Both end on `stop`.
    pub fn spawn_background(&self, stop: CancellationToken) -> Vec<JoinHandle<()>> {
        let mut out = Vec::new();
        let tagger = self.tagger();
        if tagger.is_useful() {
            info!(
                lastfm = tagger.lastfm.is_some(),
                musicbrainz = tagger.musicbrainz.is_some(),
                ollama = tagger.ollama.is_some(),
                "genre tagger started"
            );
            out.push(tokio::spawn(tagger.run(stop.clone())));
        }
        if self.enabled() {
            out.push(tokio::spawn(taste::run(self.inner.store.clone(), TASTE_INTERVAL, stop)));
        }
        out
    }

    pub(crate) fn tagger(&self) -> tagger::Tagger {
        tagger::Tagger {
            store: self.inner.store.clone(),
            lastfm: self.inner.lastfm.clone(),
            musicbrainz: self.inner.musicbrainz.clone(),
            ollama: self.inner.ollama.clone(),
            artist_ttl: self.inner.settings.cache_ttl,
            wake: self.inner.tagger_wake.clone(),
        }
    }

    /// Quick pre-check for `/ai` and the panel's Start button (no job is created).
    pub async fn check_radio_allowed(&self, owner_id: u64) -> Result<(), RadioError> {
        if !self.enabled() {
            return Err(RadioError::Disabled);
        }
        if !self.inner.store.history_enabled(owner_id).await? {
            return Err(RadioError::HistoryDisabled);
        }
        Ok(())
    }

    /// Start a radio: phases on `job.phases`, an early algorithmic pick on `job.first_pick()`,
    /// the rest on `job.result()`.
    pub fn start_radio(&self, req: RadioRequest) -> RadioJob {
        self.spawn_job(req, true)
    }

    /// More picks for a running radio (queue running low). Same as `start_radio` without the
    /// early pick; pass every queued and recently played key in `recent_keys`.
    pub fn refill(&self, req: RadioRequest) -> RadioJob {
        self.spawn_job(req, false)
    }

    fn spawn_job(&self, req: RadioRequest, with_first: bool) -> RadioJob {
        let id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        let (phase_tx, phases) = watch::channel(AiPhase::Queued);
        let (first_tx, first_rx) = oneshot::channel();
        let inner = self.inner.clone();
        let c = cancel.clone();
        let handle = tokio::spawn(async move {
            let first = with_first.then_some(first_tx);
            let r = radio::run(&inner, &req, &c, &phase_tx, first).await;
            let r = if c.is_cancelled() {
                Err(RadioError::Cancelled)
            } else {
                r
            };
            let final_phase = match &r {
                Ok(_) => AiPhase::Ready,
                Err(RadioError::Cancelled) => AiPhase::Cancelled,
                Err(e) => {
                    warn!(job = %id, error = %e, "radio job failed");
                    AiPhase::Failed
                }
            };
            let _ = phase_tx.send(final_phase);
            r
        });
        RadioJob {
            id,
            phases,
            first: with_first.then_some(first_rx),
            cancel,
            handle: Some(handle),
        }
    }

    /// Known genres of a track for the now-playing card. Reads cached tags only; never waits on
    /// any API. Empty when the track is not tagged yet.
    pub async fn genres_for(&self, track: &TrackInfo) -> Vec<String> {
        self.inner
            .store
            .track_genres_by_key(&track_key(track), 3)
            .await
            .unwrap_or_default()
    }

    /// Make sure a track is in the catalog (queued for tagging) and wake the tagger.
    pub async fn enqueue_tagging(&self, track: &TrackInfo) {
        if self.inner.store.ensure_track(track).await.is_ok() {
            self.inner.tagger_wake.notify_one();
        }
    }

    /// Wake the tagger after tracks were added to the catalog elsewhere (e.g. `record_play`).
    pub fn notify_new_tracks(&self) {
        self.inner.tagger_wake.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn inner(&self) -> &EngineInner {
        &self.inner
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use std::path::PathBuf;

    use crate::config::Config;

    pub fn config(dir: PathBuf) -> Config {
        Config {
            discord_token: String::new(),
            discord_client_id: String::new(),
            discord_client_secret: String::new(),
            node_tokens: Vec::new(),
            public_url: String::new(),
            http_host: "127.0.0.1".into(),
            http_port: 0,
            data_dir: dir.clone(),
            appdata_dir: dir,
            bot_locale: "en".into(),
            spotify_client_id: None,
            spotify_client_secret: None,
            lastfm_api_key: None,
            musicbrainz_user_agent: None,
            ai_enabled: true,
            ollama_url: None,
            ollama_model: None,
            ollama_keep_alive: "30m".into(),
            ollama_timeout_secs: 30,
            ytdlp_cookies_file: None,
            ytdlp_limit_rate: None,
            empty_channel_timeout_secs: 0,
            max_concurrent_extractions: 1,
            stream_cache_max_mb: 10,
            cache_ttl_days: 7,
            landing_url: None,
            terms_url: None,
            privacy_url: None,
            support_url: None,
        }
    }
}
