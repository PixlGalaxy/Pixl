//! Ollama client: structured JSON chat (`format` = JSON schema), model preload and `/api/ps`.
//!
//! One GPU: a global semaphore allows one request at a time. Every method that talks to the model
//! takes the permit by reference, so callers hold it for exactly as long as they need it and it is
//! released as soon as their future is dropped (cancellation drops the HTTP request too).

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::{ApiError, Cancelled};

#[derive(Clone)]
pub struct Ollama {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    url: String,
    model: String,
    keep_alive: Value,
    timeout: Duration,
    sem: Arc<Semaphore>,
    /// Whether the model reasons before answering (`/api/show` capabilities); then `think: false`
    /// is sent, since structured answers do not need it and it multiplies latency.
    thinking: tokio::sync::OnceCell<bool>,
}

/// Proof of holding the GPU slot.
pub struct OllamaPermit(#[allow(dead_code)] OwnedSemaphorePermit);

impl Ollama {
    pub fn new(http: reqwest::Client, url: &str, model: &str, keep_alive: &str, timeout: Duration) -> Self {
        // "-1" / "3600" are seconds; "30m" / "1h" are duration strings.
        let keep_alive = match keep_alive.trim().parse::<i64>() {
            Ok(n) => json!(n),
            Err(_) if keep_alive.trim().is_empty() => json!("30m"),
            Err(_) => json!(keep_alive.trim()),
        };
        Self {
            inner: Arc::new(Inner {
                http,
                url: base_url(url),
                model: model.to_owned(),
                keep_alive,
                timeout: timeout.max(Duration::from_secs(5)),
                sem: Arc::new(Semaphore::new(1)),
                thinking: tokio::sync::OnceCell::new(),
            }),
        }
    }

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

    /// Whether the model is loaded in memory (`/api/ps`). None when Ollama does not answer.
    pub async fn is_loaded(&self) -> Option<bool> {
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
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .ok()?;
        let ps: Ps = res.error_for_status().ok()?.json().await.ok()?;
        let want = with_tag(&self.inner.model);
        Some(
            ps.models
                .iter()
                .any(|m| with_tag(&m.name) == want || with_tag(&m.model) == want),
        )
    }

    async fn supports_thinking(&self) -> bool {
        #[derive(Deserialize)]
        struct Show {
            #[serde(default)]
            capabilities: Vec<String>,
        }
        let r = self
            .inner
            .thinking
            .get_or_try_init(|| async {
                let res = self
                    .inner
                    .http
                    .post(format!("{}/api/show", self.inner.url))
                    .json(&json!({ "model": self.inner.model }))
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await?
                    .error_for_status()?;
                let show: Show = res.json().await?;
                Ok::<bool, reqwest::Error>(show.capabilities.iter().any(|c| c == "thinking"))
            })
            .await;
        r.copied().unwrap_or(false)
    }

    /// Load the model (empty generate request). Returns how long it took.
    pub async fn warm_up(&self, _permit: &OllamaPermit, c: &CancellationToken) -> Result<Duration, ApiError> {
        let body = json!({ "model": self.inner.model, "keep_alive": self.inner.keep_alive });
        let start = Instant::now();
        self.post::<Value>("/api/generate", &body, c).await?;
        Ok(start.elapsed())
    }

    /// One non-streaming chat turn constrained to `schema`; the reply is parsed as `T`.
    pub async fn chat_json<T: DeserializeOwned>(
        &self,
        _permit: &OllamaPermit,
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
            "model": self.inner.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "stream": false,
            "format": schema,
            "keep_alive": self.inner.keep_alive,
            "options": { "temperature": 0.4 },
        });
        if self.supports_thinking().await {
            body["think"] = json!(false);
        }
        let r: ChatResp = self.post("/api/chat", &body, c).await?;
        debug!(
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

fn with_tag(name: &str) -> String {
    if name.contains(':') {
        name.to_owned()
    } else {
        format!("{name}:latest")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_tags_and_keep_alive() {
        assert_eq!(with_tag("llama3"), "llama3:latest");
        assert_eq!(with_tag("qwen3:8b"), "qwen3:8b");
        let o = Ollama::new(reqwest::Client::new(), "http://x/", "m", "-1", Duration::from_secs(1));
        assert_eq!(o.inner.keep_alive, json!(-1));
        assert_eq!(o.inner.url, "http://x");
        assert_eq!(base_url("10.0.0.5:11434/"), "http://10.0.0.5:11434");
        assert_eq!(base_url("https://gpu.lan"), "https://gpu.lan");
        let o = Ollama::new(reqwest::Client::new(), "http://x", "m", "30m", Duration::from_secs(1));
        assert_eq!(o.inner.keep_alive, json!("30m"));
    }

    #[tokio::test]
    async fn permit_is_exclusive_and_released_on_cancel() {
        let o = Ollama::new(reqwest::Client::new(), "http://x", "m", "30m", Duration::from_secs(1));
        let p = o.try_acquire().unwrap();
        assert!(o.try_acquire().is_none());
        let c = CancellationToken::new();
        let waiter = {
            let o = o.clone();
            let c = c.clone();
            tokio::spawn(async move { o.acquire(&c).await.is_ok() })
        };
        c.cancel();
        assert!(!waiter.await.unwrap());
        drop(p);
        assert_eq!(o.available_permits(), 1);
    }

    /// Live: needs OLLAMA_URL and OLLAMA_MODEL. `cargo test ollama_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn ollama_live() {
        let url = std::env::var("OLLAMA_URL").unwrap_or_default();
        let model = std::env::var("OLLAMA_MODEL").unwrap_or_default();
        if url.is_empty() || model.is_empty() {
            eprintln!("OLLAMA_URL / OLLAMA_MODEL not set, skipping");
            return;
        }
        let o = Ollama::new(reqwest::Client::new(), &url, &model, "30m", Duration::from_secs(300));
        let c = CancellationToken::new();
        let loaded = o.is_loaded().await;
        println!("model loaded before the test: {loaded:?}");
        assert!(loaded.is_some(), "Ollama did not answer /api/ps");
        let p = o.acquire(&c).await.unwrap();
        let first = o.warm_up(&p, &c).await.unwrap();
        println!(
            "warm_up #1 ({}): {first:?}",
            if loaded == Some(true) {
                "already loaded"
            } else {
                "cold load"
            }
        );
        let second = o.warm_up(&p, &c).await.unwrap();
        println!("warm_up #2 (warm): {second:?}");
        assert_eq!(o.is_loaded().await, Some(true));

        #[derive(serde::Deserialize, Debug)]
        struct Resp {
            genres: Vec<String>,
        }
        let schema = json!({
            "type": "object",
            "properties": { "genres": { "type": "array", "items": { "type": "string", "enum": crate::ai::taxonomy::GENRES }, "minItems": 1, "maxItems": 3 } },
            "required": ["genres"]
        });
        for (artist, title) in [("Bad Bunny", "Tití Me Preguntó"), ("Metallica", "Enter Sandman")] {
            let start = Instant::now();
            let r: Resp = o
                .chat_json(
                    &p,
                    "You are a music librarian. Answer only with JSON.",
                    &format!("Song: \"{title}\" by \"{artist}\". Pick 1 to 3 genres from the allowed list."),
                    &schema,
                    &c,
                )
                .await
                .unwrap();
            println!("classify {artist} - {title} in {:?}: {:?}", start.elapsed(), r.genres);
            assert!(!r.genres.is_empty());
        }
        // Cancelling mid-request returns at once and frees the slot.
        let c2 = CancellationToken::new();
        let o2 = o.clone();
        let cc = c2.clone();
        drop(p);
        let task = tokio::spawn(async move {
            let p = o2.acquire(&cc).await.unwrap();
            let long = "Write 50 genre names, one by one, with a long explanation of each.";
            o2.chat_json::<serde_json::Value>(&p, "Answer with JSON.", long, &json!({"type": "object"}), &cc)
                .await
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        let start = Instant::now();
        c2.cancel();
        let r = task.await.unwrap();
        println!("cancel returned in {:?}: {:?}", start.elapsed(), r.as_ref().err());
        assert!(matches!(r, Err(ApiError::Cancelled)));
        assert_eq!(o.available_permits(), 1);
    }
}
