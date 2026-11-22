use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

use super::*;

/// What the fake does with the next `/api/generate` (load) request.
#[derive(Clone, Copy)]
enum Load {
    Ok,
    OutOfMemory,
    /// Fails, but another workload loads this model meanwhile.
    OutOfMemoryButLoaded(&'static str),
}

#[derive(Default)]
struct FakeState {
    loaded: Mutex<Vec<String>>,
    loads: Mutex<VecDeque<Load>>,
    thinking: Vec<String>,
    /// "ps", "load:<model>", "show:<model>", "chat:<model>"
    calls: Mutex<Vec<String>>,
    chats: Mutex<Vec<Value>>,
}

impl FakeState {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn loads(&self) -> usize {
        self.calls().iter().filter(|c| c.starts_with("load:")).count()
    }
    fn ps_calls(&self) -> usize {
        self.calls().iter().filter(|c| *c == "ps").count()
    }
}

/// Minimal scripted Ollama: `/api/ps`, `/api/generate` (load), `/api/show`, `/api/chat`.
async fn fake(state: Arc<FakeState>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let head_end = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap_or(0))
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body: Value = serde_json::from_slice(&buf[head_end..]).unwrap_or(Value::Null);
                let model = body["model"].as_str().unwrap_or("").to_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                let (status, reply) = if path.starts_with("/api/ps") {
                    state.calls.lock().unwrap().push("ps".into());
                    let models: Vec<Value> = state
                        .loaded
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|m| json!({ "name": m, "model": m }))
                        .collect();
                    (200, json!({ "models": models }))
                } else if path.starts_with("/api/generate") {
                    state.calls.lock().unwrap().push(format!("load:{model}"));
                    let next = state.loads.lock().unwrap().pop_front().unwrap_or(Load::Ok);
                    let oom =
                        json!({ "error": "model requires more system memory (30.1 GiB) than is available (9.8 GiB)" });
                    match next {
                        Load::Ok => {
                            state.loaded.lock().unwrap().push(model.clone());
                            (200, json!({ "model": model, "done": true, "done_reason": "load" }))
                        }
                        Load::OutOfMemory => (500, oom),
                        Load::OutOfMemoryButLoaded(other) => {
                            state.loaded.lock().unwrap().push(other.into());
                            (500, oom)
                        }
                    }
                } else if path.starts_with("/api/show") {
                    state.calls.lock().unwrap().push(format!("show:{model}"));
                    let mut caps = vec!["completion"];
                    if state.thinking.contains(&model) {
                        caps.push("thinking");
                    }
                    (200, json!({ "capabilities": caps }))
                } else if path.starts_with("/api/chat") {
                    state.calls.lock().unwrap().push(format!("chat:{model}"));
                    state.chats.lock().unwrap().push(body);
                    (
                        200,
                        json!({ "message": { "role": "assistant", "content": "{\"ok\":true}" }, "done": true }),
                    )
                } else {
                    (404, json!({ "error": "not found" }))
                };
                let reply = reply.to_string();
                let res = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = sock.write_all(res.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

async fn setup(
    preferred: &str,
    checked: &[&str],
    loaded: &[&str],
    loads: &[Load],
    retry: Duration,
) -> (Ollama, Arc<FakeState>) {
    setup_with(preferred, checked, loaded, loads, retry, &[]).await
}

async fn setup_with(
    preferred: &str,
    checked: &[&str],
    loaded: &[&str],
    loads: &[Load],
    retry: Duration,
    thinking: &[&str],
) -> (Ollama, Arc<FakeState>) {
    let state = Arc::new(FakeState {
        loaded: Mutex::new(strings(loaded)),
        loads: Mutex::new(loads.iter().copied().collect()),
        thinking: strings(thinking),
        ..Default::default()
    });
    let url = fake(state.clone()).await;
    let o = Ollama::new(
        reqwest::Client::new(),
        &url,
        preferred,
        &strings(checked),
        "30m",
        Duration::from_secs(10),
        retry,
    );
    (o, state)
}

/// Runs a selection and returns it with the stages it went through.
async fn select(o: &Ollama, patience: Patience) -> (Option<SelectedModel>, Vec<SelectStage>) {
    let stages = Mutex::new(Vec::new());
    let permit = o.try_acquire().expect("GPU free");
    let slot = o
        .select_model(permit, patience, &CancellationToken::new(), &|s| {
            stages.lock().unwrap().push(s)
        })
        .await
        .unwrap();
    let model = slot.map(|s| s.model.clone());
    (model, stages.into_inner().unwrap())
}

const LONG: Duration = Duration::from_secs(30);

#[test]
fn model_names_and_keep_alive() {
    assert_eq!(normalize_name("llama3"), "llama3:latest");
    assert_eq!(normalize_name(" Qwen3:27B "), "qwen3:27b");
    assert_eq!(normalize_name("qwen3:latest"), "qwen3:latest");
    assert_eq!(normalize_name("host:5000/library/m"), "host:5000/library/m:latest");
    assert_eq!(normalize_name("hf.co/org/model:Q4_K_M"), "hf.co/org/model:q4_k_m");
    let o = Ollama::new(
        reqwest::Client::new(),
        "http://x/",
        "m",
        &[],
        "-1",
        Duration::from_secs(1),
        LONG,
    );
    assert_eq!(o.inner.keep_alive, json!(-1));
    assert_eq!(o.inner.url, "http://x");
    assert_eq!(base_url("10.0.0.5:11434/"), "http://10.0.0.5:11434");
    assert_eq!(base_url("https://gpu.lan"), "https://gpu.lan");
    let o = Ollama::new(
        reqwest::Client::new(),
        "http://x",
        "m",
        &[],
        "30m",
        Duration::from_secs(1),
        LONG,
    );
    assert_eq!(o.inner.keep_alive, json!("30m"));
    // The preferred model and duplicates are dropped from the alternates, order is kept.
    let o = Ollama::new(
        reqwest::Client::new(),
        "http://x",
        "qwen3",
        &strings(&["qwen3:latest", " gemma3:27b", "", "llama3", "Gemma3:27b"]),
        "30m",
        Duration::from_secs(1),
        LONG,
    );
    assert_eq!(o.inner.checked, strings(&["gemma3:27b", "llama3"]));
    assert!(is_memory_error(
        "model requires more system memory (30 GiB) than is available"
    ));
    assert!(is_memory_error("CUDA error: out of memory"));
    assert!(!is_memory_error("model 'x' not found"));
}

#[tokio::test]
async fn permit_is_exclusive_and_released_on_cancel() {
    let o = Ollama::new(
        reqwest::Client::new(),
        "http://x",
        "m",
        &[],
        "30m",
        Duration::from_secs(1),
        LONG,
    );
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

#[tokio::test]
async fn preferred_loaded_is_used() {
    // Ollama reports the implicit tag: "qwen3" == "qwen3:latest".
    let (o, st) = setup("qwen3", &["gemma3:27b"], &["gemma3:27b", "qwen3:latest"], &[], LONG).await;
    let (m, stages) = select(&o, Patience::Wait).await;
    let m = m.unwrap();
    assert_eq!((m.name.as_str(), m.preferred), ("qwen3", true));
    assert!(stages.is_empty());
    assert_eq!(st.loads(), 0);
}

#[tokio::test]
async fn loaded_checked_model_is_used_without_loading() {
    let (o, st) = setup(
        "qwen3:32b",
        &["gemma3:27b", "llama3"],
        &["big:70b", "llama3:latest", "gemma3:27b"],
        &[],
        LONG,
    )
    .await;
    let (m, stages) = select(&o, Patience::Wait).await;
    let m = m.unwrap();
    assert_eq!(m.name, "gemma3:27b", "first acceptable model in list order");
    assert!(!m.preferred);
    assert!(stages.is_empty());
    assert_eq!(st.loads(), 0, "nothing is loaded");
}

#[tokio::test]
async fn preferred_is_loaded_when_nothing_acceptable_is() {
    let (o, st) = setup("qwen3:8b", &["gemma3:27b"], &["big:70b"], &[Load::Ok], LONG).await;
    let (m, stages) = select(&o, Patience::Wait).await;
    assert_eq!(m.unwrap().name, "qwen3:8b");
    assert_eq!(stages, vec![SelectStage::Loading]);
    assert_eq!(st.calls().iter().filter(|c| *c == "load:qwen3:8b").count(), 1);
}

#[tokio::test]
async fn failed_load_uses_a_checked_model_that_appeared() {
    let (o, st) = setup(
        "qwen3:32b",
        &["gemma3:27b"],
        &[],
        &[Load::OutOfMemoryButLoaded("gemma3:27b")],
        LONG,
    )
    .await;
    let start = Instant::now();
    let (m, stages) = select(&o, Patience::Wait).await;
    assert_eq!(m.unwrap().name, "gemma3:27b");
    assert_eq!(stages, vec![SelectStage::Loading], "no wait");
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(st.loads(), 1);
}

#[tokio::test]
async fn failed_load_waits_and_retries() {
    let (o, st) = setup(
        "qwen3:32b",
        &["gemma3:27b"],
        &[],
        &[Load::OutOfMemory, Load::Ok],
        Duration::from_millis(100),
    )
    .await;
    let start = Instant::now();
    let (m, stages) = select(&o, Patience::Wait).await;
    let m = m.unwrap();
    assert_eq!((m.name.as_str(), m.preferred), ("qwen3:32b", true));
    assert_eq!(
        stages,
        vec![SelectStage::Loading, SelectStage::Waiting, SelectStage::Loading]
    );
    assert!(start.elapsed() >= Duration::from_millis(100));
    assert_eq!(st.loads(), 2);
}

#[tokio::test]
async fn both_loads_fail_and_there_is_no_model() {
    let (o, st) = setup(
        "qwen3:32b",
        &["gemma3:27b"],
        &["big:70b"],
        &[Load::OutOfMemory, Load::OutOfMemory],
        Duration::from_millis(100),
    )
    .await;
    let (m, stages) = select(&o, Patience::Wait).await;
    assert!(m.is_none());
    assert_eq!(
        stages,
        vec![SelectStage::Loading, SelectStage::Waiting, SelectStage::Loading]
    );
    assert_eq!(st.loads(), 2);
    assert_eq!(o.available_permits(), 1, "permit released");
    assert!(o.state().incident, "reported once");
    // Right after the incident: only /api/ps is asked, nothing is loaded again.
    let (m, stages) = select(&o, Patience::Wait).await;
    assert!(m.is_none() && stages.is_empty());
    assert_eq!(st.loads(), 2);
    // Once the retry window passed, loading is tried again (and works).
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (m, _) = select(&o, Patience::Wait).await;
    assert_eq!(m.unwrap().name, "qwen3:32b");
    assert!(!o.state().incident, "incident over");
}

#[tokio::test]
async fn quick_selection_never_waits() {
    let (o, st) = setup("qwen3:32b", &["gemma3:27b"], &[], &[Load::OutOfMemory, Load::Ok], LONG).await;
    let start = Instant::now();
    let (m, stages) = select(&o, Patience::Quick).await;
    assert!(m.is_none());
    assert_eq!(stages, vec![SelectStage::Loading], "one load, no wait");
    assert!(start.elapsed() < Duration::from_secs(5));
    // Deferred: no new load until the retry window passes.
    let (m, stages) = select(&o, Patience::Quick).await;
    assert!(m.is_none() && stages.is_empty());
    assert_eq!(st.loads(), 1);
    // A radio job (patient) still tries.
    let (m, _) = select(&o, Patience::Wait).await;
    assert_eq!(m.unwrap().name, "qwen3:32b");
}

#[tokio::test]
async fn quick_selection_uses_a_loaded_model() {
    let (o, st) = setup("qwen3:32b", &["gemma3:27b"], &["gemma3:27b"], &[], LONG).await;
    let (m, stages) = select(&o, Patience::Quick).await;
    assert_eq!(m.unwrap().name, "gemma3:27b");
    assert!(stages.is_empty());
    assert_eq!(st.loads(), 0);
}

#[tokio::test]
async fn unreachable_ollama_gives_no_model() {
    let o = Ollama::new(
        reqwest::Client::new(),
        "http://127.0.0.1:9",
        "m",
        &[],
        "30m",
        Duration::from_secs(1),
        LONG,
    );
    let (m, stages) = select(&o, Patience::Wait).await;
    assert!(m.is_none() && stages.is_empty());
}

#[tokio::test]
async fn cancel_while_waiting_to_retry() {
    let (o, _st) = setup("qwen3:32b", &[], &[], &[Load::OutOfMemory], LONG).await;
    let c = CancellationToken::new();
    let task = {
        let o = o.clone();
        let c = c.clone();
        tokio::spawn(async move {
            let p = o.try_acquire().unwrap();
            o.select_model(p, Patience::Wait, &c, &|_| {})
                .await
                .map(|s| s.is_some())
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.cancel();
    let r = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r, Err(Cancelled));
    assert_eq!(o.available_permits(), 1);
}

#[tokio::test]
async fn chat_uses_the_selection_thinking_and_cache() {
    let (o, st) = setup_with(
        "qwen3:32b",
        &["gemma3:27b"],
        &["gemma3:27b"],
        &[],
        LONG,
        &["gemma3:27b"],
    )
    .await;
    let c = CancellationToken::new();
    for _ in 0..2 {
        let slot = o
            .select_model(o.try_acquire().unwrap(), Patience::Wait, &c, &|_| {})
            .await
            .unwrap()
            .unwrap();
        let r: Value = o
            .chat_json(&slot, "s", "u", &json!({ "type": "object" }), &c)
            .await
            .unwrap();
        assert_eq!(r, json!({ "ok": true }));
    }
    let chats = st.chats.lock().unwrap().clone();
    assert_eq!(chats.len(), 2);
    for body in &chats {
        assert_eq!(body["model"], "gemma3:27b");
        assert_eq!(body["think"], json!(false), "the model thinks: turn it off");
        assert!(body.get("keep_alive").is_none(), "alternates keep their own keep_alive");
    }
    assert_eq!(st.ps_calls(), 1, "second selection came from the cache");
    assert_eq!(st.calls().iter().filter(|c| c.starts_with("show:")).count(), 1);

    // A model without thinking: no `think` field; the preferred one gets our keep_alive.
    let (o, st) = setup("qwen3:8b", &[], &["qwen3:8b"], &[], LONG).await;
    let slot = o
        .select_model(o.try_acquire().unwrap(), Patience::Wait, &c, &|_| {})
        .await
        .unwrap()
        .unwrap();
    let _: Value = o
        .chat_json(&slot, "s", "u", &json!({ "type": "object" }), &c)
        .await
        .unwrap();
    let body = st.chats.lock().unwrap()[0].clone();
    assert!(body.get("think").is_none());
    assert_eq!(body["keep_alive"], "30m");
}

/// Live: needs OLLAMA_URL and OLLAMA_MODEL; OLLAMA_CHECKED_MODELS is optional (defaults to
/// whatever is loaded). Loads nothing unless no acceptable model is in memory and
/// OLLAMA_LIVE_LOAD=1 is set (then the preferred model is loaded, as the radio would).
/// `cargo test ollama_live -- --ignored --nocapture`
#[tokio::test]
#[ignore]
async fn ollama_live() {
    let url = std::env::var("OLLAMA_URL").unwrap_or_default();
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_default();
    if url.is_empty() || model.is_empty() {
        eprintln!("OLLAMA_URL / OLLAMA_MODEL not set, skipping");
        return;
    }
    let probe = Ollama::new(
        reqwest::Client::new(),
        &url,
        &model,
        &[],
        "30m",
        Duration::from_secs(300),
        LONG,
    );
    let mut loaded = probe.loaded_models().await.expect("Ollama did not answer /api/ps");
    println!("/api/ps: {loaded:?}");
    let preferred_loaded = loaded.iter().any(|m| normalize_name(m) == normalize_name(&model));
    println!("preferred model loaded: {preferred_loaded}");
    let mut checked: Vec<String> = std::env::var("OLLAMA_CHECKED_MODELS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    if checked.is_empty() {
        checked = loaded.clone();
    }
    let make = || {
        Ollama::new(
            reqwest::Client::new(),
            &url,
            &model,
            &checked,
            "30m",
            Duration::from_secs(300),
            LONG,
        )
    };
    let c = CancellationToken::new();
    let o = make();
    if o.pick_loaded(&loaded).is_none() {
        if std::env::var("OLLAMA_LIVE_LOAD").is_err() {
            println!("no acceptable model is loaded; set OLLAMA_LIVE_LOAD=1 to let the test load one, skipping");
            return;
        }
        let stages = Mutex::new(Vec::new());
        let start = Instant::now();
        let slot = o
            .select_model(o.acquire(&c).await.unwrap(), Patience::Wait, &c, &|s| {
                stages.lock().unwrap().push(s)
            })
            .await
            .unwrap();
        println!(
            "nothing acceptable loaded: selection took {:?}, stages {:?}, got {:?}",
            start.elapsed(),
            stages.lock().unwrap(),
            slot.as_ref().map(|s| (&s.model.name, s.model.preferred))
        );
        assert!(slot.is_some(), "the preferred model loads on an idle GPU");
        drop(slot);
        loaded = probe.loaded_models().await.expect("Ollama did not answer /api/ps");
        println!("/api/ps after loading: {loaded:?}");
    }
    // A fresh client (no cached selection) picks the loaded model and loads nothing.
    let o = make();
    let stages = Mutex::new(Vec::new());
    let start = Instant::now();
    let slot = o
        .select_model(o.acquire(&c).await.unwrap(), Patience::Wait, &c, &|s| {
            stages.lock().unwrap().push(s)
        })
        .await
        .unwrap()
        .expect("a loaded acceptable model is selected");
    println!(
        "selected {} (preferred: {}, thinks: {}) in {:?}, stages {:?}",
        slot.model.name,
        slot.model.preferred,
        slot.model.think,
        start.elapsed(),
        stages.lock().unwrap()
    );
    assert!(stages.lock().unwrap().is_empty(), "nothing was loaded");
    assert!(loaded
        .iter()
        .any(|m| normalize_name(m) == normalize_name(&slot.model.name)));

    #[derive(serde::Deserialize, Debug)]
    struct Resp {
        genres: Vec<String>,
    }
    let schema = json!({
        "type": "object",
        "properties": { "genres": { "type": "array", "items": { "type": "string", "enum": crate::ai::taxonomy::GENRES }, "minItems": 1, "maxItems": 3 } },
        "required": ["genres"]
    });
    let start = Instant::now();
    let r: Resp = o
        .chat_json(
            &slot,
            "You are a music librarian. Answer only with JSON.",
            "Song: \"Enter Sandman\" by \"Metallica\". Pick 1 to 3 genres from the allowed list.",
            &schema,
            &c,
        )
        .await
        .unwrap();
    println!("classify in {:?}: {:?}", start.elapsed(), r.genres);
    assert!(!r.genres.is_empty());
    drop(slot);
    assert_eq!(o.available_permits(), 1);
}
