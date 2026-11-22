use std::time::Duration;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::*;
use crate::{
    ai::{test_util::config, AiEngine},
    procs::ProcessRegistry,
    sources::Sources,
    store::{test_util::track, Feedback, Tag, TagSource, UserPrefsPatch},
};

async fn engine_with(store: &Store, tweak: impl FnOnce(&mut crate::config::Config)) -> AiEngine {
    let dir = std::env::temp_dir().join(format!("lynx-ai-{}", uuid::Uuid::new_v4()));
    let mut cfg = config(dir);
    tweak(&mut cfg);
    let sources = Sources::new(&cfg, store.clone(), ProcessRegistry::new()).unwrap();
    AiEngine::new(&cfg, store.clone(), sources)
}

async fn play(store: &Store, user: u64, key: &str, artist: &str, genre: &str, times: u32) {
    let mut t = track(key);
    t.artists = vec![artist.into()];
    let mut id = None;
    for _ in 0..times {
        id = store.record_play(user, &t).await.unwrap();
    }
    let tag = Tag {
        tag: genre.into(),
        weight: 1.0,
        source: TagSource::Lastfm,
    };
    store.write_track_tags(id.unwrap(), &[tag]).await.unwrap();
}

fn history_off() -> UserPrefsPatch {
    UserPrefsPatch {
        locale: None,
        history_enabled: Some(false),
    }
}

/// Owner 1 likes rock (12 tracks by 6 artists); neighbor 2 plays other rock tracks; listener 3
/// dislikes one of them; listener 4 has history off.
async fn fixture() -> Store {
    let store = Store::open_in_memory().await.unwrap();
    for i in 0..12 {
        play(
            &store,
            1,
            &format!("own{i}"),
            &format!("Rocker {}", i / 2),
            "rock",
            1 + i % 4,
        )
        .await;
    }
    for i in 0..10 {
        play(&store, 2, &format!("nb{i}"), &format!("Band {}", i % 5), "rock", 2).await;
    }
    play(&store, 3, "nb0", "Band 0", "rock", 1).await;
    store
        .set_feedback(3, &track("nb0"), Some(Feedback::Dislike))
        .await
        .unwrap();
    store.replace_neighbors(1, &[(2, 0.9)]).await.unwrap();
    play(&store, 4, "metal1", "Metal Guy", "metal", 5).await;
    store.update_user_prefs(4, &history_off()).await.unwrap();
    store
}

fn request(owner: u64) -> RadioRequest {
    RadioRequest {
        guild_id: 10,
        channel_id: 20,
        owner_id: owner,
        listener_ids: vec![3, 4],
        prompt: None,
        recent_keys: vec!["nb1".into(), "own0".into()],
        batch_size: 10,
        mix: RadioMix::Balanced,
        blend: true,
    }
}

fn key(p: &RadioPick) -> String {
    track_key(&p.track)
}

#[tokio::test]
async fn algorithmic_radio_respects_filters_and_mix() {
    let store = fixture().await;
    let engine = engine_with(&store, |_| {}).await;
    let mut job = engine.start_radio(request(1));
    let first = job.first_pick().await.expect("early pick");
    let picks = job.result().await.unwrap();
    assert_eq!(picks.len(), 10);

    let mut all = vec![first];
    all.extend(picks);
    let keys: Vec<String> = all.iter().map(key).collect();
    let unique: HashSet<&String> = keys.iter().collect();
    assert_eq!(unique.len(), keys.len(), "no duplicates: {keys:?}");
    assert!(!keys.contains(&"nb0".to_string()), "a listener's dislike is excluded");
    assert!(
        !keys.contains(&"nb1".to_string()) && !keys.contains(&"own0".to_string()),
        "recent excluded"
    );
    assert!(
        !keys.contains(&"metal1".to_string()),
        "history-off listener does not count"
    );
    for w in all.windows(10) {
        let mut per: HashMap<&str, usize> = HashMap::new();
        for p in w {
            *per.entry(p.track.artists[0].as_str()).or_default() += 1;
        }
        assert!(per.values().all(|n| *n <= 2), "artist diversity: {per:?}");
    }
    // Both discover and favorites made it in, with their reasons.
    assert!(all.iter().any(|p| p.reason.key == ReasonKey::SimilarListeners));
    assert!(all.iter().any(|p| p.reason.key == ReasonKey::YourFavorite));
    assert!(all.iter().all(|p| p.genres == vec!["rock".to_string()]));
}

#[tokio::test]
async fn without_blend_listener_dislikes_still_filter() {
    let store = fixture().await;
    let engine = engine_with(&store, |_| {}).await;
    let mut req = request(1);
    req.blend = false;
    req.mix = RadioMix::Favorites;
    let mut job = engine.start_radio(req);
    let first = job.first_pick().await.expect("early pick");
    let mut all = vec![first];
    all.extend(job.result().await.unwrap());
    assert!(!all.iter().any(|p| key(p) == "nb0"), "listener 3's dislike is excluded");
    // Favorites mix: the owner's own tracks dominate.
    let own = all.iter().filter(|p| p.reason.key == ReasonKey::YourFavorite).count();
    assert!(own * 2 >= all.len(), "{own} of {}", all.len());
}

#[tokio::test]
async fn refill_has_no_first_pick() {
    let store = fixture().await;
    let engine = engine_with(&store, |_| {}).await;
    let mut job = engine.refill(request(1));
    assert!(job.first_pick().await.is_none());
    assert_eq!(job.result().await.unwrap().len(), 10);
}

#[tokio::test]
async fn cold_start_uses_prompt_genres() {
    let store = fixture().await;
    for i in 0..8 {
        play(
            &store,
            30 + i,
            &format!("reg{i}"),
            &format!("Perreo {i}"),
            "reggaeton",
            3,
        )
        .await;
    }
    play(&store, 50, "solo", "Someone", "pop", 1).await;
    let engine = engine_with(&store, |_| {}).await;
    let mut req = request(50);
    req.listener_ids.clear();
    req.prompt = Some("reggaetón para bailar".into());
    let mut job = engine.start_radio(req);
    let first = job.first_pick().await.unwrap();
    let picks = job.result().await.unwrap();
    assert_eq!(first.reason, Reason::matches_prompt("reggaetón para bailar"));
    let reg = picks
        .iter()
        .filter(|p| p.genres.contains(&"reggaeton".to_string()))
        .count();
    assert!(reg >= 5, "{:?}", picks.iter().map(key).collect::<Vec<_>>());
}

#[tokio::test]
async fn cold_start_without_prompt_uses_popular() {
    let store = fixture().await;
    let engine = engine_with(&store, |_| {}).await;
    let mut req = request(99);
    req.listener_ids.clear();
    req.recent_keys.clear();
    let mut job = engine.start_radio(req);
    // No history at all: globally popular tracks.
    assert!(job.first_pick().await.is_some());
    assert!(!job.result().await.unwrap().is_empty());
}

#[tokio::test]
async fn history_disabled_and_ai_disabled() {
    let store = fixture().await;
    store.update_user_prefs(1, &history_off()).await.unwrap();
    let engine = engine_with(&store, |_| {}).await;
    assert_eq!(engine.check_radio_allowed(1).await, Err(RadioError::HistoryDisabled));
    let mut job = engine.start_radio(request(1));
    assert!(job.first_pick().await.is_none());
    let mut phases = job.phases.clone();
    assert_eq!(job.result().await.unwrap_err(), RadioError::HistoryDisabled);
    assert_eq!(*phases.borrow_and_update(), AiPhase::Failed);

    let engine = engine_with(&store, |c| c.ai_enabled = false).await;
    assert_eq!(engine.check_radio_allowed(2).await, Err(RadioError::Disabled));
    assert_eq!(
        engine.start_radio(request(2)).result().await.unwrap_err(),
        RadioError::Disabled
    );
}

#[derive(Clone)]
enum Chat {
    Hang,
    Reply(String),
}

/// Minimal Ollama: `/api/ps` says the model is loaded, `/api/chat` hangs or replies.
async fn fake_ollama(chat: Chat) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let chat = chat.clone();
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
                let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                let body = if path.starts_with("/api/ps") {
                    r#"{"models":[{"name":"test:latest","model":"test:latest"}]}"#.to_owned()
                } else if path.starts_with("/api/chat") {
                    match chat {
                        Chat::Hang => {
                            std::future::pending::<()>().await;
                            unreachable!()
                        }
                        Chat::Reply(content) => {
                            serde_json::json!({ "message": { "role": "assistant", "content": content }, "done": true })
                                .to_string()
                        }
                    }
                } else {
                    r#"{"done":true}"#.to_owned()
                };
                let res = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(res.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

async fn engine_with_ollama(store: &Store, chat: Chat) -> AiEngine {
    let url = fake_ollama(chat).await;
    engine_with(store, |c| {
        c.ollama_url = Some(url);
        c.ollama_model = Some("test".into());
        c.ollama_timeout_secs = 20;
    })
    .await
}

#[tokio::test]
async fn cancel_while_thinking_releases_the_gpu() {
    let store = fixture().await;
    let engine = engine_with_ollama(&store, Chat::Hang).await;
    let mut job = engine.start_radio(request(1));
    assert!(
        job.first_pick().await.is_some(),
        "the early pick does not wait for the model"
    );
    let mut phases = job.phases.clone();
    tokio::time::timeout(Duration::from_secs(10), phases.wait_for(|p| *p == AiPhase::Thinking))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        engine.inner().ollama.as_ref().unwrap().available_permits(),
        0,
        "job holds the GPU"
    );
    job.cancel();
    let token = job.cancel_token();
    assert!(token.is_cancelled());
    assert_eq!(job.result().await.unwrap_err(), RadioError::Cancelled);
    assert_eq!(*phases.borrow(), AiPhase::Cancelled);
    assert_eq!(
        engine.inner().ollama.as_ref().unwrap().available_permits(),
        1,
        "GPU released"
    );
}

#[tokio::test]
async fn cancel_while_waiting_in_line() {
    let store = fixture().await;
    let engine = engine_with_ollama(&store, Chat::Hang).await;
    let held = engine.inner().ollama.as_ref().unwrap().try_acquire().unwrap();
    let job = engine.refill(request(1));
    let mut phases = job.phases.clone();
    // Reaches the model step and waits for the GPU (phase goes back to queued).
    tokio::time::timeout(Duration::from_secs(10), async {
        phases.wait_for(|p| *p == AiPhase::FindingSongs).await.unwrap();
        phases.wait_for(|p| *p == AiPhase::Queued).await.unwrap();
    })
    .await
    .unwrap();
    job.cancel();
    assert_eq!(job.result().await.unwrap_err(), RadioError::Cancelled);
    drop(held);
    assert_eq!(engine.inner().ollama.as_ref().unwrap().available_permits(), 1);
}

#[tokio::test]
async fn invalid_model_answers_fall_back_to_the_algorithm() {
    for reply in [r#"{"ids":["zz","c999"]}"#, "not json", r#"{"ids":[]}"#] {
        let store = fixture().await;
        let engine = engine_with_ollama(&store, Chat::Reply(reply.into())).await;
        let job = engine.refill(request(1));
        let phases = job.phases.clone();
        let picks = job.result().await.unwrap();
        assert_eq!(picks.len(), 10, "{reply}");
        assert_eq!(*phases.borrow(), AiPhase::Ready);
    }
}

#[tokio::test]
async fn valid_model_order_is_used() {
    let store = fixture().await;
    // Algorithmic order first (no model), to learn what c3 / c1 are.
    let plain = engine_with(&store, |_| {}).await;
    let algo = plain.refill(request(1)).result().await.unwrap();
    let engine = engine_with_ollama(
        &store,
        Chat::Reply(r#"{"ids":["c3","c3","c1","c2","c4","c5","bogus"]}"#.into()),
    )
    .await;
    let picks = engine.refill(request(1)).result().await.unwrap();
    assert_eq!(picks.len(), 10);
    assert_eq!(key(&picks[0]), key(&algo[2]), "model order wins");
    assert_eq!(key(&picks[1]), key(&algo[0]));
}

/// Live end to end: Last.fm tagging + similar, YouTube resolution, Ollama when configured.
/// Needs LASTFM_API_KEY (OLLAMA_URL / OLLAMA_MODEL optional).
#[tokio::test]
#[ignore]
async fn radio_live() {
    let Some(key) = std::env::var("LASTFM_API_KEY").ok().filter(|k| !k.is_empty()) else {
        eprintln!("LASTFM_API_KEY not set, skipping");
        return;
    };
    let ollama_url = std::env::var("OLLAMA_URL").ok().filter(|v| !v.is_empty());
    let ollama_model = std::env::var("OLLAMA_MODEL").ok().filter(|v| !v.is_empty());
    let store = Store::open_in_memory().await.unwrap();
    let seeds = [
        ("Bad Bunny", "Tití Me Preguntó"),
        ("KAROL G", "PROVENZA"),
        ("Feid", "Normal"),
        ("Rauw Alejandro", "Todo de Ti"),
        ("Peso Pluma", "Ella Baila Sola"),
        ("Shakira", "Hips Don't Lie"),
        ("Daddy Yankee", "Gasolina"),
        ("Ozuna", "Taki Taki"),
        ("J Balvin", "Mi Gente"),
        ("Myke Towers", "LALA"),
        ("Sech", "Otro Trago"),
    ];
    for (i, (artist, title)) in seeds.iter().enumerate() {
        let mut t = track(&format!("seed{i:02}xxxxx"));
        t.title = (*title).into();
        t.artists = vec![(*artist).into()];
        t.origin = TrackOrigin::YoutubeMusic;
        for _ in 0..(1 + i % 3) {
            store.record_play(1, &t).await.unwrap();
        }
    }
    let engine = engine_with(&store, |c| {
        c.lastfm_api_key = Some(key);
        c.ollama_url = ollama_url;
        c.ollama_model = ollama_model;
        c.ollama_timeout_secs = 180;
    })
    .await;

    // Tag the seeds like the background worker would.
    let tagger = engine.tagger();
    let stop = CancellationToken::new();
    let start = std::time::Instant::now();
    for t in store.pending_tag_tracks(50).await.unwrap() {
        let tags = tagger.tag_one_for_test(&t, &stop).await;
        println!(
            "tags {} - {}: {:?}",
            t.artists[0],
            t.title,
            tags.iter().map(|t| (&t.tag, t.weight)).collect::<Vec<_>>()
        );
        store.write_track_tags(t.id, &tags).await.unwrap();
    }
    println!("tagging took {:?}", start.elapsed());

    for prompt in [None, Some("algo de rock en español de los 90")] {
        let mut req = request(1);
        req.listener_ids.clear();
        req.recent_keys.clear();
        req.batch_size = 12;
        req.prompt = prompt.map(str::to_owned);
        let start = std::time::Instant::now();
        let mut job = engine.start_radio(req);
        let mut phases = job.phases.clone();
        let watcher = tokio::spawn(async move {
            let t0 = std::time::Instant::now();
            let mut seen = vec![(*phases.borrow_and_update(), t0.elapsed())];
            while phases.changed().await.is_ok() {
                seen.push((*phases.borrow_and_update(), t0.elapsed()));
            }
            seen
        });
        let first = job.first_pick().await;
        println!(
            "\nprompt {prompt:?}: first pick after {:?}: {:?}",
            start.elapsed(),
            first.as_ref().map(|p| (&p.track.title, &p.reason.key))
        );
        let picks = job.result().await.unwrap();
        println!("{} picks in {:?}", picks.len(), start.elapsed());
        for p in &picks {
            println!(
                "  {} - {} [{:?}] {:?} {:?}",
                p.track.artists.join(", "),
                p.track.title,
                p.track.youtube_id,
                p.genres,
                p.reason
            );
        }
        println!("phases: {:?}", watcher.await.unwrap());
        assert!(!picks.is_empty());
    }
}
