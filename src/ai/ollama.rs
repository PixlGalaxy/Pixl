//! Ollama client: model selection, structured JSON chat (`format` = JSON schema) and `/api/ps`.
//!
//! One GPU: a global semaphore allows one request at a time. [`Ollama::select_model`] takes the
//! permit and returns an [`LlmSlot`] (permit + chosen model); every chat call borrows the slot, so
//! callers hold the GPU for exactly as long as they need it and it is released as soon as their
//! future is dropped (cancellation drops the HTTP request too).
//!
//! The GPU may be shared with other workloads, so the model is chosen before each use: the
//! preferred model (`OLLAMA_MODEL`) when it is loaded, else an acceptable alternate that is already
//! loaded (`OLLAMA_CHECKED_MODELS`), else the preferred model is loaded, with one delayed retry
//! (`OLLAMA_LOAD_RETRY_SECS`). Other models are never unloaded.

use std::{
    collections::HashMap,
    fmt,
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{ApiError, Cancelled};

/// How long a selection is reused without asking `/api/ps` again (dropped early on chat errors).
const SELECTION_TTL: Duration = Duration::from_secs(30);
/// Load budget of background work (the tagger), which never waits for the GPU.
const QUICK_LOAD: Duration = Duration::from_secs(20);
const PS_TIMEOUT: Duration = Duration::from_secs(3);
const SHOW_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Ollama {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    url: String,
    /// Preferred model (`OLLAMA_MODEL`).
    model: String,
    /// Acceptable alternates, in order of preference, without the preferred model.
    checked: Vec<String>,
    keep_alive: Value,
    timeout: Duration,
    load_retry: Duration,
    sem: Arc<Semaphore>,
    /// Per model (normalized name): whether it reasons before answering (`/api/show`
    /// capabilities); then `think: false` is sent, since structured answers do not need it.
    thinking: Mutex<HashMap<String, bool>>,
    state: Mutex<SelectState>,
    /// Last `/api/ps` answer: when, and whether Ollama answered (health endpoint).
    last_ps: Mutex<Option<(Instant, bool)>>,
}

#[derive(Default)]
struct SelectState {
    cached: Option<(SelectedModel, Instant)>,
    /// A patient selection found no usable model; loads are not retried for `load_retry`.
    failed_at: Option<Instant>,
    /// A quick (background) load failed; background work does not retry it for `load_retry`.
    quick_failed_at: Option<Instant>,
    /// No model was usable and that was logged: warn once per incident.
    incident: bool,
}

/// Proof of holding the GPU slot.
pub struct OllamaPermit(#[allow(dead_code)] OwnedSemaphorePermit);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedModel {
    /// Name sent to Ollama.
    pub name: String,
    /// `OLLAMA_MODEL` (true), or an alternate that another workload already had loaded.
    pub preferred: bool,
    think: bool,
}

/// The GPU slot and the model to use while holding it.
pub struct LlmSlot {
    _permit: OllamaPermit,
    pub model: SelectedModel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Patience {
    /// Radio jobs: load the preferred model, and on failure wait `load_retry` and retry once.
    Wait,
    /// Background work: use a loaded model or try one short load; never waits.
    Quick,
}

/// Progress of a selection, for the radio phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectStage {
    Loading,
    /// A load failed; waiting `load_retry` before checking again.
    Waiting,
}

#[derive(Debug)]
enum LoadError {
    Cancelled,
    Timeout,
    OutOfMemory(String),
    Failed(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Timeout => f.write_str("timed out"),
            Self::OutOfMemory(m) => write!(f, "not enough memory: {m}"),
            Self::Failed(m) => f.write_str(m),
        }
    }
}

impl Ollama {
    pub fn new(
        http: reqwest::Client,
        url: &str,
        model: &str,
        checked: &[String],
        keep_alive: &str,
        timeout: Duration,
        load_retry: Duration,
    ) -> Self {
        // "-1" / "3600" are seconds; "30m" / "1h" are duration strings.
        let keep_alive = match keep_alive.trim().parse::<i64>() {
            Ok(n) => json!(n),
            Err(_) if keep_alive.trim().is_empty() => json!("30m"),
            Err(_) => json!(keep_alive.trim()),
        };
        let model = model.trim().to_owned();
        let mut seen = vec![normalize_name(&model)];
        let mut alternates = Vec::new();
        for m in checked.iter().map(|m| m.trim()).filter(|m| !m.is_empty()) {
            let n = normalize_name(m);
            if !seen.contains(&n) {
                seen.push(n);
                alternates.push(m.to_owned());
            }
        }
        Self {
            inner: Arc::new(Inner {
                http,
                url: base_url(url),
                model,
                checked: alternates,
                keep_alive,
                timeout: timeout.max(Duration::from_secs(5)),
                load_retry,
                sem: Arc::new(Semaphore::new(1)),
                thinking: Mutex::new(HashMap::new()),
                state: Mutex::new(SelectState::default()),
                last_ps: Mutex::new(None),
            }),
        }
    }

    /// The preferred model.
    pub fn model(&self) -> &str {
        &self.inner.model
    }

    /// Free GPU slots (0 or 1).
    pub fn available_permits(&self) -> usize {
        self.inner.sem.available_permits()
    }

    pub fn try_acquire(&self) -> Option<OllamaPermit> {
        self.inner.sem.clone().try_acquire_owned().ok().map(OllamaPermit)
    }

    /// Wait in line for the GPU.
    pub async fn acquire(&self, c: &CancellationToken) -> Result<OllamaPermit, Cancelled> {
        tokio::select! {
            p = self.inner.sem.clone().acquire_owned() => p.map(OllamaPermit).map_err(|_| Cancelled),
            _ = c.cancelled() => Err(Cancelled),
        }
    }

    /// Names of the models loaded in memory (`/api/ps`). None when Ollama does not answer.
    pub async fn loaded_models(&self) -> Option<Vec<String>> {
        let out = self.fetch_loaded_models().await;
        if let Ok(mut last) = self.inner.last_ps.lock() {
            *last = Some((Instant::now(), out.is_some()));
        }
        out
    }

    /// Whether Ollama answers. Reuses the last `/api/ps` answer when it is younger than `max_age`,
    /// else asks `/api/ps` (lists loaded models, never loads one).
    pub async fn reachable(&self, max_age: Duration) -> bool {
        let recent = self
            .inner
            .last_ps
            .lock()
            .ok()
            .and_then(|l| *l)
            .filter(|(at, _)| at.elapsed() < max_age);
        match recent {
            Some((_, ok)) => ok,
            None => self.loaded_models().await.is_some(),
        }
    }

    async fn fetch_loaded_models(&self) -> Option<Vec<String>> {
        #[derive(Deserialize)]
        struct Ps {
            #[serde(default)]
            models: Vec<PsModel>,
        }
        #[derive(Deserialize)]
        struct PsModel {
            #[serde(default)]
            name: String,
            #[serde(default)]
            model: String,
        }
        let res = self
            .inner
            .http
            .get(format!("{}/api/ps", self.inner.url))
            .timeout(PS_TIMEOUT)
            .send()
            .await
            .ok()?;
        let ps: Ps = res.error_for_status().ok()?.json().await.ok()?;
        let mut out: Vec<String> = Vec::new();
        for m in ps.models {
            for n in [m.name, m.model] {
                if !n.is_empty() && !out.iter().any(|o| normalize_name(o) == normalize_name(&n)) {
                    out.push(n);
                }
            }
        }
        Some(out)
    }

    /// Choose the model for one use of the GPU (see the module docs). None: no model is usable
    /// right now (Ollama unreachable, or the GPU is taken) and the permit is released.
    pub async fn select_model(
        &self,
        permit: OllamaPermit,
        patience: Patience,
        c: &CancellationToken,
        on_stage: &(dyn Fn(SelectStage) + Send + Sync),
    ) -> Result<Option<LlmSlot>, Cancelled> {
        let cached = self
            .state()
            .cached
            .as_ref()
            .filter(|(_, at)| at.elapsed() < SELECTION_TTL)
            .map(|(m, _)| m.clone());
        let model = match cached {
            Some(m) => Some(m),
            None => self.select_uncached(patience, c, on_stage).await?,
        };
        Ok(model.map(|model| LlmSlot { _permit: permit, model }))
    }

    async fn select_uncached(
        &self,
        patience: Patience,
        c: &CancellationToken,
        on_stage: &(dyn Fn(SelectStage) + Send + Sync),
    ) -> Result<Option<SelectedModel>, Cancelled> {
        let Some(loaded) = cancellable(c, self.loaded_models()).await? else {
            debug!("Ollama did not answer /api/ps");
            return Ok(None);
        };
        if let Some(pick) = self.pick_loaded(&loaded) {
            return self.finish(pick, c).await.map(Some);
        }
        if self.recently_failed(patience) {
            return Ok(None);
        }
        let (budget, mut tries) = match patience {
            Patience::Wait => (self.inner.timeout, 2),
            Patience::Quick => (QUICK_LOAD.min(self.inner.timeout), 1),
        };
        let mut last_err;
        loop {
            on_stage(SelectStage::Loading);
            match self.load_preferred(budget, c).await {
                Ok(took) => {
                    debug!(ms = took.as_millis() as u64, "model loaded");
                    return self.finish((self.inner.model.clone(), true), c).await.map(Some);
                }
                Err(LoadError::Cancelled) => return Err(Cancelled),
                Err(e) => {
                    debug!(error = %e, "preferred model did not load");
                    last_err = e;
                }
            }
            // Another workload may have loaded an acceptable model meanwhile.
            if let Some(pick) = self.check_loaded(c).await? {
                return self.finish(pick, c).await.map(Some);
            }
            tries -= 1;
            if tries == 0 {
                break;
            }
            on_stage(SelectStage::Waiting);
            tokio::select! {
                _ = tokio::time::sleep(self.inner.load_retry) => {}
                _ = c.cancelled() => return Err(Cancelled),
            }
            if let Some(pick) = self.check_loaded(c).await? {
                return self.finish(pick, c).await.map(Some);
            }
        }
        let err = last_err.to_string();
        let mut st = self.state();
        match patience {
            Patience::Wait => {
                st.failed_at = Some(Instant::now());
                if !st.incident {
                    st.incident = true;
                    warn!(error = %err, "no Ollama model available (GPU busy?); the radio uses only the algorithm");
                }
            }
            Patience::Quick => {
                st.quick_failed_at = Some(Instant::now());
                debug!(error = %err, "no Ollama model available for background tagging");
            }
        }
        Ok(None)
    }

    /// A selection just failed: do not try loading again until `load_retry` has passed.
    fn recently_failed(&self, patience: Patience) -> bool {
        let st = self.state();
        let recent = |t: Option<Instant>| t.is_some_and(|t| t.elapsed() < self.inner.load_retry);
        recent(st.failed_at) || (patience == Patience::Quick && recent(st.quick_failed_at))
    }

    async fn check_loaded(&self, c: &CancellationToken) -> Result<Option<(String, bool)>, Cancelled> {
        Ok(cancellable(c, self.loaded_models())
            .await?
            .and_then(|l| self.pick_loaded(&l)))
    }

    /// The preferred model if loaded, else the first checked alternate (in list order) that is.
    fn pick_loaded(&self, loaded: &[String]) -> Option<(String, bool)> {
        let loaded: Vec<(String, &String)> = loaded.iter().map(|n| (normalize_name(n), n)).collect();
        let want = normalize_name(&self.inner.model);
        if loaded.iter().any(|(n, _)| *n == want) {
            return Some((self.inner.model.clone(), true));
        }
        self.inner.checked.iter().find_map(|alt| {
            let alt = normalize_name(alt);
            loaded
                .iter()
                .find(|(n, _)| *n == alt)
                .map(|(_, name)| ((*name).clone(), false))
        })
    }

    async fn finish(
        &self,
        (name, preferred): (String, bool),
        c: &CancellationToken,
    ) -> Result<SelectedModel, Cancelled> {
        let think = self.supports_thinking(&name, c).await?;
        let m = SelectedModel { name, preferred, think };
        let mut st = self.state();
        st.cached = Some((m.clone(), Instant::now()));
        st.failed_at = None;
        st.quick_failed_at = None;
        if std::mem::take(&mut st.incident) {
            info!(model = %m.name, "Ollama model available again");
        }
        Ok(m)
    }

    async fn supports_thinking(&self, name: &str, c: &CancellationToken) -> Result<bool, Cancelled> {
        #[derive(Deserialize)]
        struct Show {
            #[serde(default)]
            capabilities: Vec<String>,
        }
        let key = normalize_name(name);
        if let Some(t) = self.lock_thinking().get(&key).copied() {
            return Ok(t);
        }
        let fetch = async {
            let res = self
                .inner
                .http
                .post(format!("{}/api/show", self.inner.url))
                .json(&json!({ "model": name }))
                .timeout(SHOW_TIMEOUT)
                .send()
                .await?
                .error_for_status()?;
            let show: Show = res.json().await?;
            Ok::<bool, reqwest::Error>(show.capabilities.iter().any(|c| c == "thinking"))
        };
        match cancellable(c, fetch).await? {
            Ok(t) => {
                self.lock_thinking().insert(key, t);
                Ok(t)
            }
            Err(e) => {
                debug!(error = %e, "ollama /api/show failed");
                Ok(false)
            }
        }
    }

    /// Load the preferred model (empty generate request). Returns how long it took.
    async fn load_preferred(&self, budget: Duration, c: &CancellationToken) -> Result<Duration, LoadError> {
        let body = json!({ "model": self.inner.model, "keep_alive": self.inner.keep_alive, "stream": false });
        let req = self
            .inner
            .http
            .post(format!("{}/api/generate", self.inner.url))
            .json(&body)
            .timeout(budget);
        let start = Instant::now();
        let fut = async {
            let res = req.send().await.map_err(load_error)?;
            let status = res.status();
            let text = res.text().await.map_err(load_error)?;
            let error = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error")?.as_str().map(str::to_owned));
            if status.is_success() && error.is_none() {
                return Ok(start.elapsed());
            }
            let msg = error.unwrap_or_else(|| format!("HTTP {status}: {}", text.chars().take(200).collect::<String>()));
            Err(if is_memory_error(&msg) {
                LoadError::OutOfMemory(msg)
            } else {
                LoadError::Failed(msg)
            })
        };
        tokio::select! {
            r = fut => r,
            _ = c.cancelled() => Err(LoadError::Cancelled),
        }
    }

    /// One non-streaming chat turn constrained to `schema`; the reply is parsed as `T`.
    pub async fn chat_json<T: DeserializeOwned>(
        &self,
        slot: &LlmSlot,
        system: &str,
        user: &str,
        schema: &Value,
        c: &CancellationToken,
    ) -> Result<T, ApiError> {
        let r = self.chat_inner(&slot.model, system, user, schema, c).await;
        if matches!(&r, Err(e) if !matches!(e, ApiError::Cancelled)) {
            // The model may have been unloaded by another workload: look again next time.
            self.state().cached = None;
        }
        r
    }

    async fn chat_inner<T: DeserializeOwned>(
        &self,
        model: &SelectedModel,
        system: &str,
        user: &str,
        schema: &Value,
        c: &CancellationToken,
    ) -> Result<T, ApiError> {
        #[derive(Deserialize)]
        struct ChatResp {
            message: Option<Msg>,
            #[serde(default)]
            load_duration: u64,
            #[serde(default)]
            total_duration: u64,
        }
        #[derive(Deserialize)]
        struct Msg {
            #[serde(default)]
            content: String,
        }
        let mut body = json!({
            "model": model.name,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "stream": false,
            "format": schema,
            "options": { "temperature": 0.4 },
        });
        // An alternate belongs to another workload: do not impose our keep_alive on it.
        if model.preferred {
            body["keep_alive"] = self.inner.keep_alive.clone();
        }
        if model.think {
            body["think"] = json!(false);
        }
        let r: ChatResp = self.post("/api/chat", &body, c).await?;
        debug!(
            model = %model.name,
            load_ms = r.load_duration / 1_000_000,
            total_ms = r.total_duration / 1_000_000,
            "ollama chat"
        );
        let content = r.message.map(|m| m.content).unwrap_or_default();
        serde_json::from_str(content.trim()).map_err(|e| ApiError::Api(format!("invalid JSON from the model: {e}")))
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &Value, c: &CancellationToken) -> Result<T, ApiError> {
        let req = self
            .inner
            .http
            .post(format!("{}{path}", self.inner.url))
            .json(body)
            .timeout(self.inner.timeout);
        // Dropping the request future on cancel closes the connection, which makes Ollama stop.
        let fut = async {
            let res = req.send().await?;
            let status = res.status();
            if !status.is_success() {
                let text = res.text().await.unwrap_or_default();
                return Err(ApiError::Http(format!(
                    "Ollama HTTP {status}: {}",
                    text.chars().take(200).collect::<String>()
                )));
            }
            Ok(res.json::<T>().await?)
        };
        tokio::select! {
            r = fut => r,
            _ = c.cancelled() => Err(ApiError::Cancelled),
        }
    }

    fn state(&self) -> MutexGuard<'_, SelectState> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_thinking(&self) -> MutexGuard<'_, HashMap<String, bool>> {
        self.inner.thinking.lock().unwrap_or_else(|e| e.into_inner())
    }
}

async fn cancellable<T>(c: &CancellationToken, fut: impl Future<Output = T>) -> Result<T, Cancelled> {
    tokio::select! {
        r = fut => Ok(r),
        _ = c.cancelled() => Err(Cancelled),
    }
}

fn load_error(e: reqwest::Error) -> LoadError {
    if e.is_timeout() {
        LoadError::Timeout
    } else {
        LoadError::Failed(e.to_string())
    }
}

/// "model requires more system memory (…) than is available", "insufficient memory",
/// "CUDA error: out of memory", "cudaMalloc failed", …
fn is_memory_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("memory") || m.contains("insufficient") || m.contains("alloc")
}

/// "host:11434" -> "http://host:11434" (Ollama's own CLI accepts URLs without a scheme).
fn base_url(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    if u.contains("://") {
        u.to_owned()
    } else {
        format!("http://{u}")
    }
}

/// Comparable model name: trimmed, lowercase, with the implicit `:latest` tag
/// ("Qwen3" == "qwen3:latest"; a registry port like "host:5000/m" is not a tag).
fn normalize_name(name: &str) -> String {
    let n = name.trim().to_lowercase();
    let has_tag = n.rsplit('/').next().is_some_and(|last| last.contains(':'));
    if has_tag {
        n
    } else {
        format!("{n}:latest")
    }
}

#[cfg(test)]
mod tests;
