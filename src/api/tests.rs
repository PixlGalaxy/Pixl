//! Router tests with an in-memory store and a fake Discord (no network).

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use serenity::{async_trait, cache::Cache, http::Http};
use songbird::Songbird;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::{
    router,
    upstream::{ChannelKind, Discord, GuildChannel, GuildSummary, OAuthUser, Profile, UpstreamError},
    ApiState, Options,
};
use crate::{
    ai::AiEngine,
    discord::registry::{BotInfo, BotRegistry},
    i18n::I18n,
    player::{PlayerDeps, PlayerManager},
    procs::ProcessRegistry,
    sources::Sources,
    store::{SessionWriter, Store},
    AppState,
};

const GUILD: u64 = 1;
const VOICE: u64 = 100;
const LOUNGE: u64 = 101;
const TEXT: u64 = 200;
const ANA: u64 = 10;
const BOSS: u64 = 11;

/// Discord stand-in: codes map to users, membership / manager / voice state are editable.
#[derive(Default)]
struct Fake {
    members: Mutex<HashMap<(u64, u64), bool>>,
    voice: Mutex<HashMap<(u64, u64), u64>>,
}

impl Fake {
    fn set_voice(&self, user: u64, channel: Option<u64>) {
        let mut v = self.voice.lock().unwrap();
        match channel {
            Some(c) => v.insert((GUILD, user), c),
            None => v.remove(&(GUILD, user)),
        };
    }
}

fn user_of(code: &str) -> Option<u64> {
    match code {
        "ana" => Some(ANA),
        "boss" => Some(BOSS),
        "stranger" => Some(12),
        _ => None,
    }
}

#[async_trait]
impl Discord for Fake {
    async fn exchange_code(&self, code: &str) -> Result<String, UpstreamError> {
        user_of(code)
            .map(|_| format!("token-{code}"))
            .ok_or(UpstreamError::Rejected)
    }

    async fn current_user(&self, token: &str) -> Result<OAuthUser, UpstreamError> {
        let code = token.strip_prefix("token-").ok_or(UpstreamError::Rejected)?;
        let id = user_of(code).ok_or(UpstreamError::Rejected)?;
        Ok(OAuthUser {
            id,
            username: code.into(),
            global_name: Some(code.to_uppercase()),
            avatar_url: Some(format!("https://cdn.discordapp.com/avatars/{id}/abc.png")),
            locale: Some("es-ES".into()),
        })
    }

    async fn member(&self, guild_id: u64, user_id: u64) -> Result<Option<Profile>, UpstreamError> {
        Ok(self
            .members
            .lock()
            .unwrap()
            .contains_key(&(guild_id, user_id))
            .then(|| Profile {
                name: format!("user{user_id}"),
                avatar_url: None,
            }))
    }

    async fn is_manager(&self, guild_id: u64, user_id: u64) -> bool {
        self.members
            .lock()
            .unwrap()
            .get(&(guild_id, user_id))
            .copied()
            .unwrap_or(false)
    }

    async fn search_members(&self, _: u64, _: &str, _: u32) -> Option<Vec<(u64, Profile)>> {
        None
    }

    async fn user(&self, _: u64) -> Option<Profile> {
        None
    }

    fn guild(&self, guild_id: u64) -> Option<GuildSummary> {
        (guild_id == GUILD).then(|| GuildSummary {
            name: "Galaxy".into(),
            icon_url: Some("https://cdn.discordapp.com/icons/1/i.png".into()),
        })
    }

    fn channel_name(&self, guild_id: u64, channel_id: u64) -> Option<String> {
        self.guild_channels(guild_id)
            .into_iter()
            .find(|c| c.id == channel_id)
            .map(|c| c.name)
    }

    fn user_voice_channel(&self, guild_id: u64, user_id: u64) -> Option<u64> {
        self.voice.lock().unwrap().get(&(guild_id, user_id)).copied()
    }

    fn voice_members(&self, guild_id: u64) -> Vec<(u64, Option<Profile>)> {
        let v = self.voice.lock().unwrap();
        let mut ids: Vec<u64> = v.keys().filter(|(g, _)| *g == guild_id).map(|(_, u)| *u).collect();
        ids.sort_unstable();
        ids.into_iter()
            .map(|u| {
                (
                    u,
                    Some(Profile {
                        name: format!("user{u}"),
                        avatar_url: None,
                    }),
                )
            })
            .collect()
    }

    fn bot_name(&self, _: usize) -> String {
        "Lynx".into()
    }

    fn guild_channels(&self, guild_id: u64) -> Vec<GuildChannel> {
        if guild_id != GUILD {
            return Vec::new();
        }
        let c = |id, name: &str, kind| GuildChannel {
            id,
            name: name.into(),
            kind,
            parent_name: Some("Music".into()),
        };
        vec![
            c(TEXT, "chat", ChannelKind::Text),
            c(VOICE, "General", ChannelKind::Voice),
            c(LOUNGE, "Lounge", ChannelKind::Voice),
        ]
    }
}

struct Harness {
    app: Router,
    fake: Arc<Fake>,
    state: ApiState,
}

async fn harness_with(session_ttl: Duration, ai_enabled: bool) -> Harness {
    let dir = std::env::temp_dir().join(format!("lynx-api-test-{}", uuid::Uuid::new_v4()));
    let mut cfg = crate::ai::test_util::config(dir);
    cfg.discord_client_id = "4242".into();
    cfg.ai_enabled = ai_enabled;
    let config = Arc::new(cfg);
    let store = Store::open_in_memory().await.unwrap();
    let i18n = Arc::new(I18n::load("en").unwrap());
    let procs = ProcessRegistry::new();
    let sources = Sources::new(&config, store.clone(), procs.clone()).unwrap();
    let (sessions, _task) = SessionWriter::spawn(store.clone());
    let bots = Arc::new(BotRegistry::new(vec![Arc::new(BotInfo::new(
        0,
        Arc::new(Http::new("x")),
        Arc::new(Cache::new()),
        Songbird::serenity(),
    ))]));
    let ai = AiEngine::new(&config, store.clone(), sources.clone());
    let players = PlayerManager::new(PlayerDeps {
        config: config.clone(),
        i18n: i18n.clone(),
        store: store.clone(),
        sources: sources.clone(),
        procs: procs.clone(),
        sessions,
        bots: bots.clone(),
        ai: ai.clone(),
        http: reqwest::Client::new(),
    });
    let app = AppState {
        config,
        store,
        i18n,
        procs,
        sources,
        bots,
        players,
        ai,
        shutdown: CancellationToken::new(),
    };
    let fake = Arc::new(Fake::default());
    {
        let mut m = fake.members.lock().unwrap();
        m.insert((GUILD, ANA), false);
        m.insert((GUILD, BOSS), true);
    }
    let state = ApiState::new(app, fake.clone(), Options { session_ttl }).unwrap();
    Harness {
        app: router(state.clone()),
        fake,
        state,
    }
}

async fn harness() -> Harness {
    harness_with(Duration::from_secs(3600), true).await
}

impl Harness {
    async fn call(&self, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        self.call_from(method, path, token, body, "203.0.113.7").await
    }

    async fn call_from(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        ip: &str,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("x-forwarded-for", ip);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        (status, value)
    }

    async fn login(&self, code: &str, channel: Option<u64>) -> (String, Value) {
        let (status, v) = self
            .call(
                "POST",
                "/api/auth",
                None,
                Some(
                    json!({ "code": code, "guildId": GUILD.to_string(), "channelId": channel.map(|c| c.to_string()) }),
                ),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        (v["session"].as_str().unwrap().to_owned(), v)
    }
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn public_config_and_health() {
    let h = harness_with(Duration::from_secs(60), false).await;
    let (s, v) = h.call("GET", "/api/public", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["clientId"], "4242");
    assert_eq!(v["features"]["ai"], false);
    assert!(v["links"].is_object());
    let (s, _) = h.call("GET", "/healthz", None, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = h.call("GET", "/index.html", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "no static files any more");
    assert_eq!(code(&v), "not_found");
}

#[tokio::test]
async fn auth_flow() {
    let h = harness().await;
    let (s, v) = h
        .call(
            "POST",
            "/api/auth",
            None,
            Some(json!({ "code": "bad", "guildId": "1", "channelId": null })),
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&v), "unauthorized");

    let (s, v) = h.call("POST", "/api/auth", None, Some(json!({ "nope": 1 }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(code(&v), "invalid");

    // Not a member of the guild it claims.
    let (s, v) = h
        .call(
            "POST",
            "/api/auth",
            None,
            Some(json!({ "code": "stranger", "guildId": "1", "channelId": "100" })),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(code(&v), "no_permission");

    h.fake.set_voice(ANA, Some(VOICE));
    let (session, v) = h.login("ana", Some(VOICE)).await;
    assert_eq!(session.len(), 64);
    assert_eq!(v["accessToken"], "token-ana");
    assert_eq!(v["user"]["id"], "10");
    assert_eq!(v["user"]["globalName"], "ANA");
    assert_eq!(v["user"]["avatarUrl"], "/backend/cdn/avatars/10/abc.png");
    assert_eq!(
        v["guild"],
        json!({ "id": "1", "name": "Galaxy", "iconUrl": "/backend/cdn/icons/1/i.png" })
    );
    assert_eq!(v["channel"], json!({ "id": "100", "name": "General" }));
    assert_eq!(
        v["permissions"],
        json!({ "isManager": false, "isDj": false, "canControl": true, "canAdd": true })
    );
    assert_eq!(v["locale"], "es-ES", "Discord locale when no panel locale is saved");

    // A channel of another guild is not echoed back.
    let (_, v) = h.login("ana", Some(999)).await;
    assert!(v["channel"].is_null());

    // The session works; no session or a made-up one does not.
    let (s, _) = h.call("GET", "/api/settings", Some(&session), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = h.call("GET", "/api/settings", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = h.call("GET", "/api/settings", Some(&"0".repeat(64)), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Kicked from the guild: access is gone once the membership cache entry expires.
    h.fake.members.lock().unwrap().remove(&(GUILD, ANA));
    h.state.members.remove(&(GUILD, ANA));
    let (s, v) = h.call("GET", "/api/settings", Some(&session), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(code(&v), "no_permission");
}

#[tokio::test]
async fn auth_is_rate_limited_per_ip() {
    let h = harness().await;
    let body = || Some(json!({ "code": "bad", "guildId": null, "channelId": null }));
    for _ in 0..10 {
        let (s, _) = h.call_from("POST", "/api/auth", None, body(), "198.51.100.1").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    let (s, v) = h.call_from("POST", "/api/auth", None, body(), "198.51.100.1").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(code(&v), "rate_limited");
    let (s, _) = h.call_from("POST", "/api/auth", None, body(), "198.51.100.2").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "another address is not limited");
}

#[tokio::test]
async fn sessions_expire() {
    let h = harness_with(Duration::ZERO, true).await;
    let (session, _) = h.login("ana", None).await;
    let (s, v) = h.call("GET", "/api/me/prefs", Some(&session), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&v), "unauthorized");
}

#[tokio::test]
async fn settings_and_dj_mode() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (boss, _) = h.login("boss", None).await;

    let (s, v) = h.call("GET", "/api/settings", Some(&ana), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["controlMode"], "everyone");
    assert_eq!(v["voteSkip"]["mode"], "percent");
    assert_eq!(v["commandChannels"], json!([]));
    assert_eq!(v["voiceChannels"], json!([]));

    // Everyone mode: ana passes the rule; nothing plays, so not_found.
    let (s, v) = h
        .call(
            "POST",
            "/api/player/pause",
            Some(&ana),
            Some(json!({ "channelId": "100" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_found"));
    // Another channel: not in it.
    let (s, v) = h
        .call(
            "POST",
            "/api/player/pause",
            Some(&ana),
            Some(json!({ "channelId": "101" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "not_in_channel"));
    let (s, v) = h.call("GET", "/api/player?channelId=100", Some(&ana), None).await;
    assert_eq!((s, v), (StatusCode::OK, Value::Null));

    // Writes are managers only.
    let (s, v) = h
        .call("PUT", "/api/settings", Some(&ana), Some(json!({ "controlMode": "dj" })))
        .await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "no_permission"));
    let (s, v) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&boss),
            Some(json!({ "defaultVolume": 999 })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"));
    let (s, v) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&boss),
            Some(json!({ "controlMode": "dj", "voteSkip": { "enabled": true } })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["controlMode"], "dj");
    assert_eq!(v["voteSkip"]["enabled"], true);
    assert_eq!(v["maxQueueLength"], 500, "PUT returns the whole object");

    // DJ mode: a listener cannot control...
    let (s, v) = h
        .call(
            "POST",
            "/api/player/pause",
            Some(&ana),
            Some(json!({ "channelId": "100" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "no_permission"));
    let (s, v) = h
        .call(
            "POST",
            "/api/queue/clear",
            Some(&ana),
            Some(json!({ "channelId": "100" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "no_permission"));
    // ...but skip is a vote (passes the rule) with vote skip on.
    let (s, _) = h
        .call(
            "POST",
            "/api/player/skip",
            Some(&ana),
            Some(json!({ "channelId": "100" })),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Making her a DJ lifts it.
    let (s, _) = h
        .call("POST", "/api/djs", Some(&ana), Some(json!({ "userId": "10" })))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = h
        .call("POST", "/api/djs", Some(&boss), Some(json!({ "userId": "99" })))
        .await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_found"), "not a member");
    let (s, _) = h
        .call("POST", "/api/djs", Some(&boss), Some(json!({ "userId": "10" })))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = h.call("GET", "/api/djs", Some(&ana), None).await;
    assert_eq!(v["djs"], json!([{ "id": "10", "name": "user10", "avatarUrl": null }]));
    let (s, _) = h
        .call(
            "POST",
            "/api/player/pause",
            Some(&ana),
            Some(json!({ "channelId": "100" })),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h.call("DELETE", "/api/djs/10", Some(&boss), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = h.call("DELETE", "/api/djs/10", Some(&boss), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Bad bodies are `invalid`.
    let (s, v) = h
        .call(
            "POST",
            "/api/player/volume",
            Some(&boss),
            Some(json!({ "channelId": "100", "volume": 500 })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"));
    let (s, v) = h
        .call(
            "POST",
            "/api/queue/move",
            Some(&boss),
            Some(json!({ "channelId": "100", "uid": "x", "toIndex": 0 })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"));
}

#[tokio::test]
async fn channel_restrictions() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (boss, _) = h.login("boss", None).await;

    let (s, _) = h.call("GET", "/api/guild/channels", Some(&ana), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = h.call("GET", "/api/guild/channels", Some(&boss), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v["text"],
        json!([{ "id": "200", "name": "chat", "parentName": "Music" }])
    );
    assert_eq!(v["voice"][1]["id"], "101");

    let (s, v) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&boss),
            Some(json!({ "voiceChannels": ["200"] })),
        )
        .await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::BAD_REQUEST, "invalid"),
        "a text channel is not a voice channel"
    );
    let (s, v) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&boss),
            Some(json!({ "voiceChannels": ["101"], "commandChannels": ["200", "100"] })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["voiceChannels"], json!(["101"]));
    assert_eq!(v["commandChannels"], json!(["100", "200"]));

    let (s, v) = h
        .call(
            "POST",
            "/api/queue/add",
            Some(&ana),
            Some(json!({ "channelId": "100", "query": "song" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "channel_not_allowed"));
    let (s, v) = h
        .call(
            "POST",
            "/api/queue/add",
            Some(&ana),
            Some(json!({ "channelId": "101", "query": "song" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "not_in_channel"));
    let (s, v) = h
        .call(
            "POST",
            "/api/queue/add",
            Some(&ana),
            Some(json!({ "channelId": "100", "query": "" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"));

    let (_, v) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&boss),
            Some(json!({ "voiceChannels": [] })),
        )
        .await;
    assert_eq!(v["voiceChannels"], json!([]));
    assert_eq!(v["commandChannels"], json!(["100", "200"]), "the other list is kept");
}

#[tokio::test]
async fn history_disabled_blocks_feedback() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let uid = uuid::Uuid::new_v4().to_string();
    let body = || Some(json!({ "channelId": "100", "uid": uid, "value": "like" }));

    let (s, _) = h.call("POST", "/api/tracks/feedback", Some(&ana), body()).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "history on: passes, nothing plays");

    let (s, v) = h
        .call(
            "PUT",
            "/api/me/prefs",
            Some(&ana),
            Some(json!({ "historyEnabled": false })),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({ "locale": null, "historyEnabled": false }));
    let (s, v) = h.call("POST", "/api/tracks/feedback", Some(&ana), body()).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "history_disabled"));

    let (s, v) = h
        .call("PUT", "/api/me/prefs", Some(&ana), Some(json!({ "locale": "es" })))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({ "locale": "es", "historyEnabled": false }));
    let (s, _) = h
        .call(
            "PUT",
            "/api/me/prefs",
            Some(&ana),
            Some(json!({ "locale": "<script>" })),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, v) = h.call("GET", "/api/me/taste", Some(&ana), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({ "topGenres": [], "topTracks": [] }));
    let (s, _) = h.call("DELETE", "/api/me/data", Some(&ana), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let (s, v) = h
        .call("POST", "/api/ai/start", Some(&ana), Some(json!({ "channelId": "100" })))
        .await;
    assert_eq!((s, code(&v)), (StatusCode::SERVICE_UNAVAILABLE, "unavailable"));
}

#[tokio::test]
async fn candidates_from_voice() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    h.fake.set_voice(13, Some(LOUNGE));
    let (boss, _) = h.login("boss", None).await;
    let (s, v) = h.call("GET", "/api/members/candidates?q=", Some(&boss), None).await;
    assert_eq!(s, StatusCode::OK);
    let ids: HashSet<&str> = v["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, HashSet::from(["10", "13"]));
    let (_, v) = h
        .call("GET", "/api/members/candidates?q=USER13", Some(&boss), None)
        .await;
    assert_eq!(
        v["members"],
        json!([{ "id": "13", "name": "user13", "avatarUrl": null }])
    );
}

#[tokio::test]
async fn media_proxies_are_restricted() {
    let h = harness().await;
    let (s, v) = h
        .call("GET", "/img?url=https%3A%2F%2Fevil.example.com%2Fa.png", None, None)
        .await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"));
    let (s, _) = h
        .call("GET", "/img?url=http%3A%2F%2Fi.ytimg.com%2Fa.jpg", None, None)
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h.call("GET", "/img", None, None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h.call("GET", "/cdn/attachments/1/2/file.png", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h.call("GET", "/cdn/avatars/..%2F..%2Fx", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

async fn next<S>(socket: &mut S) -> Value
where
    S: futures_util::Stream<
            Item = Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    use futures_util::StreamExt;
    let msg = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(msg.to_text().unwrap()).unwrap()
}

#[tokio::test]
async fn websocket_handshake() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;

    // Through the router: a bad session is refused before the upgrade is looked at.
    let req = Request::builder()
        .uri(format!("/ws?session={}&channelId=100", "f".repeat(64)))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(Body::empty())
        .unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // A real socket with a valid session gets the full state (nothing playing) and permissions.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = h.app.clone();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let url = format!("ws://{addr}/ws?session={ana}&channelId=100");
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let v = next(&mut socket).await;
    assert_eq!(v["type"], "state");
    assert!(v["state"].is_null());
    assert!(v["serverNow"].as_i64().unwrap() > 0);
    let v = next(&mut socket).await;
    assert_eq!(v["type"], "permissions");
    assert_eq!(v["permissions"]["canAdd"], true);

    // A settings change reaches the socket, with the permissions it changes.
    let (boss, _) = h.login("boss", None).await;
    h.call(
        "PUT",
        "/api/settings",
        Some(&boss),
        Some(json!({ "controlMode": "dj" })),
    )
    .await;
    let v = next(&mut socket).await;
    assert_eq!(v["type"], "settings");
    assert_eq!(v["settings"]["controlMode"], "dj");
    let v = next(&mut socket).await;
    assert_eq!(v["type"], "permissions");
    assert_eq!(v["permissions"]["canControl"], false);

    // A bad session over a real socket fails the handshake.
    let bad = format!("ws://{addr}/ws?session={}&channelId=100", "0".repeat(64));
    assert!(tokio_tungstenite::connect_async(bad).await.is_err());
}
