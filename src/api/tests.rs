//! Router tests with an in-memory store and a fake Discord (no network).

mod notices;

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
const BOBBY: u64 = 13;

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
    harness_full(session_ttl, ai_enabled, 1, super::health::LIMITS).await
}

async fn harness_full(
    session_ttl: Duration,
    ai_enabled: bool,
    bot_count: usize,
    health_limits: super::health::Limits,
) -> Harness {
    harness_opts(session_ttl, ai_enabled, bot_count, health_limits, super::seek_hold::HOLD_MAX).await
}

async fn harness_opts(
    session_ttl: Duration,
    ai_enabled: bool,
    bot_count: usize,
    health_limits: super::health::Limits,
    seek_hold_max: Duration,
) -> Harness {
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
    let bots = Arc::new(BotRegistry::new(
        (0..bot_count)
            .map(|i| {
                Arc::new(BotInfo::new(
                    i,
                    Arc::new(Http::new("x")),
                    Arc::new(Cache::new()),
                    { crate::test_support::songbird_scheduler(); Songbird::serenity() },
                ))
            })
            .collect(),
    ));
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
        ytdlp_version: Default::default(),
        notices: Default::default(),
        updates: Default::default(),
        limits: Default::default(),
        shutdown: CancellationToken::new(),
    };
    let fake = Arc::new(Fake::default());
    {
        let mut m = fake.members.lock().unwrap();
        m.insert((GUILD, ANA), false);
        m.insert((GUILD, BOSS), true);
    }
    let state = ApiState::new(
        app,
        fake.clone(),
        Options {
            session_ttl,
            health_limits,
            seek_hold_max,
        },
    )
    .unwrap();
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
            .header("x-real-ip", ip);
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

    assert_eq!(v["launchChannel"], json!({ "id": "100", "name": "General" }));

    // A channel of another guild is not echoed back as the launch channel.
    let (_, v) = h.login("ana", Some(999)).await;
    assert!(v["launchChannel"].is_null());
    assert_eq!(
        v["channel"]["id"], "100",
        "the followed channel is still her voice channel"
    );

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

    // Bad bodies are `invalid` (the channel check comes first, so boss joins the channel).
    h.fake.set_voice(BOSS, Some(VOICE));
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
async fn music_channel_and_manager_exemption() {
    let h = harness().await;
    h.fake.set_voice(BOSS, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (boss, _) = h.login("boss", Some(VOICE)).await;

    let (_, v) = h.call("GET", "/api/settings", Some(&ana), None).await;
    assert_eq!(v["musicChannel"], Value::Null);
    assert_eq!(v["managersExempt"], true);

    let put = |body: Value| Some(body);
    let (s, _) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&ana),
            put(json!({ "musicChannel": "200" })),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "managers only");
    let (s, v) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&boss),
            put(json!({ "musicChannel": "999" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"), "unknown channel");

    let (s, v) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&boss),
            put(json!({ "musicChannel": "200" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["musicChannel"], "200");
    let (_, v) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&boss),
            put(json!({ "musicChannel": "100" })),
        )
        .await;
    assert_eq!(v["musicChannel"], "100", "a voice channel's chat");
    let (_, v) = h
        .call("PUT", "/api/settings", Some(&boss), put(json!({ "controlMode": "dj" })))
        .await;
    assert_eq!(v["musicChannel"], "100", "absent = unchanged");
    let (_, v) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&boss),
            put(json!({ "musicChannel": null })),
        )
        .await;
    assert_eq!(v["musicChannel"], Value::Null);
    h.call(
        "PUT",
        "/api/settings",
        Some(&boss),
        put(json!({ "musicChannel": "200" })),
    )
    .await;
    let (_, v) = h
        .call("PUT", "/api/settings", Some(&boss), put(json!({ "musicChannel": "" })))
        .await;
    assert_eq!(v["musicChannel"], Value::Null, "empty string clears too");
    assert_eq!(h.state.app.store.music_channel(GUILD).await.unwrap(), None);

    // Playback restricted to 101: an exempt manager may still use 100, a non-exempt one may not.
    let (s, _) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&boss),
            put(json!({ "voiceChannels": ["101"] })),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let add = || Some(json!({ "channelId": "100", "query": "song" }));
    let (_, v) = h.call("POST", "/api/queue/add", Some(&boss), add()).await;
    assert_ne!(code(&v), "channel_not_allowed", "{v}");
    let (s, v) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&boss),
            put(json!({ "managersExempt": false })),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["managersExempt"], false);
    assert_eq!(v["voiceChannels"], json!(["101"]), "other rules kept");
    let (s, v) = h.call("POST", "/api/queue/add", Some(&boss), add()).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "channel_not_allowed"), "{v}");
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
    assert_eq!(
        v,
        json!({ "locale": null, "historyEnabled": false, "theme": null, "accent": null })
    );
    let (s, v) = h.call("POST", "/api/tracks/feedback", Some(&ana), body()).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "history_disabled"));

    let (s, v) = h
        .call("PUT", "/api/me/prefs", Some(&ana), Some(json!({ "locale": "es" })))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v,
        json!({ "locale": "es", "historyEnabled": false, "theme": null, "accent": null })
    );
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
    // The wipe signs every session out.
    let (ana, _) = h.login("ana", Some(VOICE)).await;

    let (s, v) = h
        .call("POST", "/api/ai/start", Some(&ana), Some(json!({ "channelId": "100" })))
        .await;
    // The radio checks the real voice state first (the fake Discord has none for it).
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, v) = h
        .call("POST", "/api/ai/stop", Some(&ana), Some(json!({ "channelId": "100" })))
        .await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_found"));
    let (s, v) = h
        .call(
            "POST",
            "/api/ai/cancel",
            Some(&ana),
            Some(json!({ "channelId": "100", "jobId": "x" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"));
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

type TestSocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Serve the router on a local port (real sockets).
async fn serve(h: &Harness) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = h.app.clone();
    tokio::spawn(async move { axum::serve(listener, app).await });
    addr
}

/// Open `/ws` (no token in the URL) and send the `auth` message first.
async fn ws_open(addr: std::net::SocketAddr, session: &str) -> TestSocket {
    use futures_util::SinkExt;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
    let hello = json!({ "type": "auth", "session": session }).to_string();
    socket
        .send(tokio_tungstenite::tungstenite::Message::text(hello))
        .await
        .unwrap();
    socket
}

/// Skip data messages until the server closes; its close code.
async fn close_code(socket: &mut TestSocket) -> Option<u16> {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), socket.next()).await.unwrap();
        match msg {
            Some(Ok(Message::Close(frame))) => return frame.map(|f| u16::from(f.code)),
            Some(Ok(_)) => {}
            _ => return None,
        }
    }
}

#[tokio::test]
async fn websocket_handshake() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let addr = serve(&h).await;

    // A real socket with a valid session (sent as the first message, never in the URL) gets the full
    // state (nothing playing) and permissions.
    let mut socket = ws_open(addr, &ana).await;
    let v = next(&mut socket).await;
    assert_eq!(
        v,
        json!({ "type": "channel", "channel": { "id": "100", "name": "General" } })
    );
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

    // A bad session in the auth message: closed with the unauthorized code (the panel renews it).
    let mut bad = ws_open(addr, &"0".repeat(64)).await;
    assert_eq!(close_code(&mut bad).await, Some(super::ws::CLOSE_UNAUTHORIZED));
    // A session in the URL is not looked at: without the auth message the socket gets nothing.
    let (mut legacy, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?session={ana}"))
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(1), futures_util::StreamExt::next(&mut legacy)).await;
    assert!(first.is_err(), "no data before the auth message");
    // A first message that is not `auth`: refused.
    {
        use futures_util::SinkExt;
        legacy
            .send(tokio_tungstenite::tungstenite::Message::text(r#"{"type":"ping"}"#))
            .await
            .unwrap();
    }
    assert_eq!(close_code(&mut legacy).await, Some(super::ws::CLOSE_UNAUTHORIZED));
}

#[tokio::test]
async fn websocket_without_auth_closes_after_the_timeout() {
    let h = harness().await;
    let addr = serve(&h).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
    assert_eq!(close_code(&mut socket).await, Some(super::ws::CLOSE_AUTH_TIMEOUT));
}

#[tokio::test]
async fn websockets_per_user_are_capped_and_closed_on_sign_out() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (other, _) = h.login("ana", Some(VOICE)).await;
    let addr = serve(&h).await;

    let mut sockets = Vec::new();
    for _ in 0..super::ws::MAX_SOCKETS_PER_USER {
        let mut s = ws_open(addr, &ana).await;
        assert_eq!(next(&mut s).await["type"], "channel");
        sockets.push(s);
    }
    assert_eq!(h.state.sockets.count(ANA), super::ws::MAX_SOCKETS_PER_USER);
    // One more: the oldest is closed, the count stays at the cap.
    let mut newest = ws_open(addr, &other).await;
    assert_eq!(next(&mut newest).await["type"], "channel");
    assert_eq!(close_code(&mut sockets[0]).await, Some(super::ws::CLOSE_REPLACED));
    assert_eq!(h.state.sockets.count(ANA), super::ws::MAX_SOCKETS_PER_USER);

    // Signing one session out closes its sockets right away, not the other session's.
    let (s, _) = h.call("DELETE", "/api/auth", Some(&ana), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(close_code(&mut sockets[1]).await, Some(super::ws::CLOSE_UNAUTHORIZED));
    let (s, _) = h.call("GET", "/api/me/channel", Some(&ana), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = h.call("GET", "/api/me/channel", Some(&other), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = h.call("DELETE", "/api/auth", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // Closed sockets unregister.
    for _ in 0..50 {
        if h.state.sockets.count(ANA) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(h.state.sockets.count(ANA), 1, "only the other session's socket is left");
}

#[tokio::test]
async fn deleting_my_data_signs_out_every_session() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (a, _) = h.login("ana", Some(VOICE)).await;
    let (b, _) = h.login("ana", Some(TEXT)).await;
    let (boss, _) = h.login("boss", None).await;
    let addr = serve(&h).await;
    let mut socket = ws_open(addr, &b).await;
    assert_eq!(next(&mut socket).await["type"], "channel");

    let (s, _) = h.call("DELETE", "/api/me/data", Some(&a), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    for t in [&a, &b] {
        let (s, _) = h.call("GET", "/api/me/prefs", Some(t), None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(close_code(&mut socket).await, Some(super::ws::CLOSE_UNAUTHORIZED));
    // Also gone from SQLite, not only from the cache.
    let restarted = ApiState::new(h.state.app.clone(), h.fake.clone(), Options::default()).unwrap();
    assert!(restarted.sessions.get(&a).await.is_none());
    assert!(restarted.sessions.get(&boss).await.is_some(), "other users stay signed in");
    // Signing in again works.
    let (_, v) = h.login("ana", Some(VOICE)).await;
    assert!(v["session"].is_string());
}

#[tokio::test]
async fn followed_channel_is_the_users_voice_channel() {
    let h = harness().await;

    // Launched from a text channel while in voice: follows the voice channel.
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, v) = h.login("ana", Some(TEXT)).await;
    assert_eq!(v["channel"], json!({ "id": "100", "name": "General" }));
    assert_eq!(v["launchChannel"], json!({ "id": "200", "name": "chat" }));
    assert_eq!(v["permissions"]["canControl"], true);

    // Launched from another voice channel: still the one she is in.
    let (_, v) = h.login("ana", Some(LOUNGE)).await;
    assert_eq!(v["channel"]["id"], "100");
    assert_eq!(v["launchChannel"]["id"], "101");

    // Not in voice: nothing to follow, wherever it was launched.
    h.fake.set_voice(ANA, None);
    let (_, v) = h.login("ana", Some(VOICE)).await;
    assert!(v["channel"].is_null(), "{v}");
    assert_eq!(v["launchChannel"]["id"], "100");
    let (_, v) = h.login("ana", Some(TEXT)).await;
    assert!(v["channel"].is_null());

    // The polling fallback follows moves.
    let (s, v) = h.call("GET", "/api/me/channel", Some(&ana), None).await;
    assert_eq!((s, v), (StatusCode::OK, json!({ "channel": null })));
    h.fake.set_voice(ANA, Some(LOUNGE));
    let (_, v) = h.call("GET", "/api/me/channel", Some(&ana), None).await;
    assert_eq!(v, json!({ "channel": { "id": "101", "name": "Lounge" } }));
    let (s, _) = h.call("GET", "/api/me/channel", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn panel_routes_only_accept_the_users_channel() {
    let h = harness().await;
    let (boss, _) = h.login("boss", Some(TEXT)).await;
    h.fake.set_voice(BOSS, Some(LOUNGE));

    // A manager in another channel cannot see or control the player of VOICE from the panel.
    let (s, v) = h.call("GET", "/api/player?channelId=100", Some(&boss), None).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "not_in_channel"));
    let (s, v) = h.call("GET", "/api/lyrics?channelId=100", Some(&boss), None).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "not_in_channel"));
    for (path, body) in [
        ("/api/player/pause", json!({ "channelId": "100" })),
        ("/api/player/stop", json!({ "channelId": "100" })),
        ("/api/queue/add", json!({ "channelId": "100", "query": "song" })),
        ("/api/queue/clear", json!({ "channelId": "100" })),
        ("/api/ai/stop", json!({ "channelId": "100" })),
    ] {
        let (s, v) = h.call("POST", path, Some(&boss), Some(body)).await;
        assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "not_in_channel"), "{path}");
    }
    // Their own channel passes the check (nothing plays there).
    let (s, v) = h.call("GET", "/api/player?channelId=101", Some(&boss), None).await;
    assert_eq!((s, v), (StatusCode::OK, Value::Null));
    let (s, v) = h
        .call(
            "POST",
            "/api/player/pause",
            Some(&boss),
            Some(json!({ "channelId": "101" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_found"));

    // Not in voice at all.
    h.fake.set_voice(BOSS, None);
    let (s, v) = h.call("GET", "/api/player?channelId=101", Some(&boss), None).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "not_in_channel"));
}

#[tokio::test]
async fn websocket_follows_voice_moves() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(TEXT)).await;
    let addr = serve(&h).await;
    let mut socket = ws_open(addr, &ana).await;
    assert_eq!(next(&mut socket).await["channel"]["id"], "100");
    assert_eq!(next(&mut socket).await["type"], "state");
    assert_eq!(next(&mut socket).await["type"], "permissions");

    // Someone else moving changes nothing for her; her own move switches the socket.
    h.fake.set_voice(BOBBY, Some(LOUNGE));
    h.state.app.bots.voice_moved(GUILD, BOBBY);
    h.fake.set_voice(ANA, Some(LOUNGE));
    h.state.app.bots.voice_moved(GUILD, ANA);
    let v = next(&mut socket).await;
    assert_eq!(
        v,
        json!({ "type": "channel", "channel": { "id": "101", "name": "Lounge" } })
    );
    let v = next(&mut socket).await;
    assert_eq!(v["type"], "state");
    assert!(v["state"].is_null());
    let v = next(&mut socket).await;
    assert_eq!(v["type"], "permissions");
    assert_eq!(v["permissions"]["canControl"], true);

    // Disconnecting from voice: `channel: null`.
    h.fake.set_voice(ANA, None);
    h.state.app.bots.voice_moved(GUILD, ANA);
    let v = next(&mut socket).await;
    assert_eq!(v, json!({ "type": "channel", "channel": null }));
    assert_eq!(next(&mut socket).await["type"], "state");
    let v = next(&mut socket).await;
    assert_eq!(v["type"], "permissions");
    assert_eq!(v["permissions"]["canControl"], false);
}

#[tokio::test]
async fn sessions_survive_a_restart_and_store_only_a_hash() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(TEXT)).await;

    // A fresh API state over the same store (a restart) accepts the token.
    let restarted = ApiState::new(h.state.app.clone(), h.fake.clone(), Options::default()).unwrap();
    assert_eq!(restarted.sessions.len(), 0, "nothing in memory yet");
    let req = Request::builder()
        .uri("/api/me/channel")
        .header("authorization", format!("Bearer {ana}"))
        .body(Body::empty())
        .unwrap();
    let res = router(restarted.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let s = restarted.sessions.get(&ana).await.unwrap();
    assert_eq!(
        (s.user_id, s.guild_id, s.launch_channel_id),
        (ANA, Some(GUILD), Some(TEXT))
    );
    assert_eq!(s.global_name.as_deref(), Some("ANA"));

    // Only the SHA-256 of the token is on disk.
    type Row = (String, i64, String, Option<String>, Option<String>);
    let rows: Vec<Row> =
        sqlx::query_as("SELECT token_hash, user_id, username, global_name, avatar_url FROM panel_sessions")
            .fetch_all(h.state.app.store.pool())
            .await
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, super::session::hash_token(&ana));
    assert_ne!(rows[0].0, ana);
    let dump = format!("{rows:?}");
    assert!(!dump.contains(&ana), "the raw token is never stored");

    // Expired rows are refused and swept.
    let short = harness_with(Duration::ZERO, true).await;
    let (dead, _) = short.login("ana", None).await;
    let restarted = ApiState::new(short.state.app.clone(), short.fake.clone(), Options::default()).unwrap();
    assert!(restarted.sessions.get(&dead).await.is_none());
    assert_eq!(short.state.app.store.sweep_panel_sessions().await.unwrap(), 1);
}

#[tokio::test]
async fn appearance_prefs_are_validated() {
    let h = harness().await;
    let (ana, _) = h.login("ana", None).await;
    let (s, v) = h
        .call(
            "PUT",
            "/api/me/prefs",
            Some(&ana),
            Some(json!({ "theme": "system", "accent": "forest" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((&v["theme"], &v["accent"]), (&json!("system"), &json!("forest")));
    for body in [
        json!({ "accent": "neon" }),
        json!({ "theme": "sepia" }),
        json!({ "accent": "" }),
    ] {
        let (s, v) = h.call("PUT", "/api/me/prefs", Some(&ana), Some(body)).await;
        assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"));
    }
    let (_, v) = h
        .call("PUT", "/api/me/prefs", Some(&ana), Some(json!({ "accent": null })))
        .await;
    assert_eq!((&v["theme"], &v["accent"]), (&json!("system"), &Value::Null));
    let (_, v) = h.call("GET", "/api/me/prefs", Some(&ana), None).await;
    assert_eq!(v["theme"], "system");
}

/// Seed history for ANA (two rock songs, one jazz, one `url:` key) and BOSS (one rock song).
async fn seed_taste(h: &Harness) {
    use crate::store::{test_util::track, Tag, TagSource};
    let store = &h.state.app.store;
    let tag = |t: &str| Tag {
        tag: t.into(),
        weight: 0.5,
        source: TagSource::Lastfm,
    };
    let mut a = track("a");
    a.thumbnail = Some("https://i.ytimg.com/vi/a/hq.jpg".into());
    let mut odd = track("odd");
    odd.youtube_id = None;
    odd.url = "https://example.com/x/y?z=1".into();
    let ra = store.record_play_at(ANA, &a, 10).await.unwrap().unwrap();
    store.record_play_at(ANA, &a, 11).await.unwrap();
    let rb = store.record_play_at(ANA, &track("b"), 20).await.unwrap().unwrap();
    let rj = store.record_play_at(ANA, &track("j"), 30).await.unwrap().unwrap();
    let ro = store.record_play_at(ANA, &odd, 5).await.unwrap().unwrap();
    let rx = store.record_play_at(BOSS, &track("x"), 40).await.unwrap().unwrap();
    for id in [ra, rb, ro, rx] {
        store.write_track_tags(id, &[tag("rock")]).await.unwrap();
    }
    store.write_track_tags(rj, &[tag("jazz")]).await.unwrap();
}

#[tokio::test]
async fn taste_genre_tracks_and_forgetting_a_song() {
    let h = harness().await;
    seed_taste(&h).await;
    let (ana, _) = h.login("ana", None).await;
    let (boss, _) = h.login("boss", None).await;

    let (s, _) = h.call("GET", "/api/me/taste/genre/rock", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, v) = h.call("GET", "/api/me/taste/genre/rock", Some(&ana), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 3);
    let keys: Vec<&str> = v["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["a", "b", "url:https://example.com/x/y?z=1"]);
    let first = &v["tracks"][0];
    assert_eq!(
        (first["plays"].clone(), first["lastPlayed"].clone()),
        (json!(2), json!(11))
    );
    assert_eq!(first["title"], "Song a");
    assert_eq!(first["artists"], json!(["Artist a"]));
    assert!(
        first["artworkUrl"].as_str().unwrap().starts_with("/backend/img?url="),
        "{first}"
    );
    assert_eq!(v["tracks"][1]["artworkUrl"], Value::Null);

    // Case and spelling go through the taxonomy; paging; unknown genres are refused.
    let (_, v) = h
        .call("GET", "/api/me/taste/genre/ROCK?limit=1&offset=1", Some(&ana), None)
        .await;
    assert_eq!(
        (v["total"].clone(), v["tracks"][0]["key"].clone()),
        (json!(3), json!("b"))
    );
    let (_, v) = h
        .call("GET", "/api/me/taste/genre/rock?limit=500", Some(&ana), None)
        .await;
    assert_eq!(
        v["tracks"].as_array().unwrap().len(),
        3,
        "limit is clamped, not refused"
    );
    let (s, v) = h
        .call("GET", "/api/me/taste/genre/not%20a%20genre", Some(&ana), None)
        .await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"));
    let (s, _) = h
        .call("GET", "/api/me/taste/genre/rock?limit=-1", Some(&ana), None)
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, v) = h.call("GET", "/api/me/taste/genre/rock", Some(&boss), None).await;
    assert_eq!(v["total"], 1, "only the caller's history");

    let (_, taste) = h.call("GET", "/api/me/taste", Some(&ana), None).await;
    assert!(taste["topTracks"][0]["artworkUrl"]
        .as_str()
        .unwrap()
        .starts_with("/backend/img"));

    // Forget: history row gone, dislike set, taste stale, other users untouched.
    let store = h.state.app.store.clone();
    for st in store.stale_tastes(10).await.unwrap() {
        store
            .put_taste(st.user_id, &Default::default(), st.version)
            .await
            .unwrap();
    }
    let (s, _) = h.call("DELETE", "/api/me/history/a", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, v) = h.call("DELETE", "/api/me/history/a", Some(&ana), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    let (_, v) = h.call("GET", "/api/me/taste/genre/rock", Some(&ana), None).await;
    assert_eq!(v["total"], 2);
    let a = crate::store::test_util::track("a");
    assert_eq!(
        store.get_feedback(ANA, &a).await.unwrap(),
        Some(crate::store::Feedback::Dislike)
    );
    let stale: Vec<u64> = store
        .stale_tastes(10)
        .await
        .unwrap()
        .iter()
        .map(|s| s.user_id)
        .collect();
    assert_eq!(stale, [ANA]);
    let (s, v) = h.call("DELETE", "/api/me/history/a", Some(&ana), None).await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_found"), "already gone");
    let (s, _) = h.call("DELETE", "/api/me/history/x", Some(&ana), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "someone else's song");
    assert_eq!(store.history_len(BOSS).await.unwrap(), 1);
    // Keys with slashes travel percent-encoded in one segment.
    let (s, v) = h
        .call(
            "DELETE",
            "/api/me/history/url%3Ahttps%3A%2F%2Fexample.com%2Fx%2Fy%3Fz%3D1",
            Some(&ana),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    assert_eq!(store.history_len(ANA).await.unwrap(), 2);

    // History off: 403 history_disabled on both routes.
    h.call(
        "PUT",
        "/api/me/prefs",
        Some(&ana),
        Some(json!({ "historyEnabled": false })),
    )
    .await;
    let (s, v) = h.call("GET", "/api/me/taste/genre/rock", Some(&ana), None).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "history_disabled"));
    let (s, v) = h.call("DELETE", "/api/me/history/b", Some(&ana), None).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "history_disabled"));
    assert_eq!(store.history_len(ANA).await.unwrap(), 2, "nothing removed");
}

#[tokio::test]
async fn taste_genre_routes_need_ai() {
    let h = harness_with(Duration::from_secs(3600), false).await;
    seed_taste(&h).await;
    let (ana, _) = h.login("ana", None).await;
    let (s, v) = h.call("GET", "/api/me/taste/genre/rock", Some(&ana), None).await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_found"));
    let (s, v) = h.call("DELETE", "/api/me/history/a", Some(&ana), None).await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_found"));
    assert_eq!(h.state.app.store.history_len(ANA).await.unwrap(), 4);
}

fn detached_player(h: &Harness, channel: u64, ids: &[&str]) -> crate::player::PlayerHandle {
    use crate::player::{QueueEntry, Requester, Restore, StartRequest};
    let tracks = ids
        .iter()
        .map(|id| Arc::new(QueueEntry::new(crate::store::test_util::track(id), Requester::id_only(ANA))))
        .collect();
    h.state.app.players.start_detached(StartRequest {
        guild_id: GUILD,
        voice_channel_id: channel,
        bot_index: 0,
        text_channel_id: None,
        started_by: ANA,
        // Nobody in the channel for the player: nothing is fetched.
        humans: HashSet::new(),
        settings: Default::default(),
        restore: Some(Restore {
            tracks,
            index: 0,
            loop_mode: Default::default(),
            volume: 100,
            position_ms: 0,
            after_restart: false,
        }),
    })
}

/// Poll a GET until `ok` (jam changes reach the players asynchronously).
async fn eventually(h: &Harness, path: &str, token: &str, ok: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..200 {
        let (s, v) = h.call("GET", path, Some(token), None).await;
        if s == StatusCode::OK && ok(&v) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{path} never matched");
}

#[tokio::test]
async fn jam_routes() {
    let h = harness().await;
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (boss, _) = h.login("boss", Some(LOUNGE)).await;
    let body = |c: u64| json!({ "channelId": c.to_string() });

    // Not in the channel / nothing playing / no such jam.
    let (s, v) = h.call("POST", "/api/jam/create", Some(&ana), Some(body(VOICE))).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "not_in_channel"));
    h.fake.set_voice(ANA, Some(VOICE));
    h.fake.set_voice(BOSS, Some(LOUNGE));
    let (s, v) = h.call("POST", "/api/jam/create", Some(&ana), Some(body(VOICE))).await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_found"));
    let (s, v) = h
        .call(
            "POST",
            "/api/jam/join",
            Some(&boss),
            Some(json!({ "channelId": "101", "jamId": "abcdefghij" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "jam_ended"));

    detached_player(&h, VOICE, &["a", "b"]);
    detached_player(&h, LOUNGE, &["x"]);
    let (s, v) = h.call("POST", "/api/jam/create", Some(&ana), Some(body(VOICE))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let id = v["jamId"].as_str().unwrap().to_owned();
    assert_eq!(
        v["shareLink"],
        format!("https://discord.com/activities/4242?custom_id=jam-{id}")
    );
    assert_eq!(
        v["jam"]["you"],
        json!({ "member": true, "isHost": true, "canManage": true, "canEdit": true })
    );
    assert_eq!(
        v["jam"]["settings"],
        json!({ "allowChannels": true, "allowServers": false, "editableBy": "everyone", "closed": false })
    );
    // Creating again returns the same jam.
    let (_, again) = h.call("POST", "/api/jam/create", Some(&ana), Some(body(VOICE))).await;
    assert_eq!(again["jamId"], id.as_str());

    // The join prompt of the link, then the join.
    let (s, v) = h.call("GET", &format!("/api/jam/jam-{id}"), Some(&boss), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["join"], json!({ "allowed": true, "code": null }));
    assert_eq!(v["jam"]["hostChannelId"], "100");
    let join = json!({ "channelId": "101", "jamId": id });
    let (s, v) = h.call("POST", "/api/jam/join", Some(&boss), Some(join.clone())).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["jam"]["members"].as_array().unwrap().len(), 2);
    assert_eq!(v["jam"]["you"]["member"], true);
    assert_eq!(v["jam"]["you"]["isHost"], false);
    let (s, v) = h.call("POST", "/api/jam/join", Some(&boss), Some(join.clone())).await;
    assert_eq!((s, code(&v)), (StatusCode::CONFLICT, "jam_already_in"));
    let (_, v) = h.call("GET", &format!("/api/jam/{id}"), Some(&boss), None).await;
    assert_eq!(v["join"]["code"], "jam_already_in");
    // The member's player state carries the jam and the shared queue.
    let state = eventually(&h, "/api/player?channelId=101", &boss, |v| {
        v["jam"]["members"].as_array().is_some_and(|m| m.len() == 2)
            && v["upcoming"].as_array().is_some_and(|u| u.len() == 2)
    })
    .await;
    assert_eq!(state["upcoming"][0]["title"], "Song a");

    // Only the host channel manages the jam.
    let (s, v) = h
        .call(
            "POST",
            "/api/jam/settings",
            Some(&boss),
            Some(json!({ "channelId": "101", "closed": true })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "jam_not_host"));
    let (s, v) = h
        .call(
            "POST",
            "/api/jam/settings",
            Some(&ana),
            Some(json!({ "channelId": "100", "editableBy": "host" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["settings"]["editableBy"], "host");
    // Host-only editing: the member cannot change the shared queue, but its volume is its own.
    let (s, v) = h.call("POST", "/api/queue/clear", Some(&boss), Some(body(LOUNGE))).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "jam_host_only"));
    let (s, v) = h
        .call(
            "POST",
            "/api/queue/add",
            Some(&boss),
            Some(json!({ "channelId": "101", "query": "x" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "jam_host_only"));
    let (s, _) = h
        .call(
            "POST",
            "/api/player/volume",
            Some(&boss),
            Some(json!({ "channelId": "101", "volume": 50 })),
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let v = eventually(&h, "/api/jam?channelId=101", &boss, |v| v["jam"]["you"]["canEdit"] == false).await;
    assert_eq!(v["jam"]["you"]["canManage"], false);

    // Close, kick, end.
    let (s, v) = h.call("POST", "/api/jam/close", Some(&ana), Some(body(VOICE))).await;
    assert_eq!((s, v["settings"]["closed"].clone()), (StatusCode::OK, json!(true)));
    let kick = |c: u64| json!({ "channelId": "100", "guildId": GUILD.to_string(), "memberChannelId": c.to_string() });
    let (s, v) = h.call("POST", "/api/jam/kick", Some(&ana), Some(kick(VOICE))).await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"));
    let (s, _) = h.call("POST", "/api/jam/kick", Some(&ana), Some(kick(LOUNGE))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // The removed channel has its own queue back.
    let state = eventually(&h, "/api/player?channelId=101", &boss, |v| v["jam"].is_null()).await;
    assert_eq!(state["current"]["title"], "Song x", "waits for people, like a restored queue");
    // Closed: it cannot come back.
    let (s, v) = h.call("POST", "/api/jam/join", Some(&boss), Some(join)).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "jam_closed"));
    let (s, v) = h.call("POST", "/api/jam/leave", Some(&boss), Some(body(LOUNGE))).await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_in_jam"));

    let (s, _) = h.call("POST", "/api/jam/end", Some(&ana), Some(body(VOICE))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    eventually(&h, "/api/jam?channelId=100", &ana, |v| v["jam"].is_null()).await;
    let (s, v) = h.call("GET", &format!("/api/jam/{id}"), Some(&boss), None).await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "jam_ended"));
    let (s, v) = h.call("GET", "/api/jam/nope", Some(&boss), None).await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"));
}

#[tokio::test]
async fn jam_join_needs_a_free_bot_and_members_can_leave() {
    let h = harness().await;
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (boss, _) = h.login("boss", Some(LOUNGE)).await;
    h.fake.set_voice(ANA, Some(VOICE));
    h.fake.set_voice(BOSS, Some(LOUNGE));
    detached_player(&h, VOICE, &["a"]);
    let (_, v) = h
        .call("POST", "/api/jam/create", Some(&ana), Some(json!({ "channelId": "100" })))
        .await;
    let id = v["jamId"].as_str().unwrap().to_owned();
    // No Lynx in the lounge and no free bot: the usual busy error (no invite: no more bots to add).
    let (s, v) = h
        .call(
            "POST",
            "/api/jam/join",
            Some(&boss),
            Some(json!({ "channelId": "101", "jamId": id })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::SERVICE_UNAVAILABLE, "bot_unavailable"), "{v}");
    assert!(v["error"].get("invite").is_none());

    // With a player there, join and leave.
    detached_player(&h, LOUNGE, &["x"]);
    let (s, _) = h
        .call(
            "POST",
            "/api/jam/join",
            Some(&boss),
            Some(json!({ "channelId": "101", "jamId": id })),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = h
        .call("POST", "/api/jam/leave", Some(&boss), Some(json!({ "channelId": "101" })))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    eventually(&h, "/api/jam?channelId=100", &ana, |v| {
        v["jam"]["members"].as_array().is_some_and(|m| m.len() == 1)
    })
    .await;
}

/// A saved Spotify playlist with its songs already in the shared cache (no network in tests:
/// Spotify is not configured, so any real fetch fails right away with "not enabled").
async fn seed_playlist(h: &Harness, user: u64) -> (String, String) {
    let url = "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M";
    let store = &h.state.app.store;
    let id = match store
        .save_playlist(
            user,
            &crate::store::NewPlaylist {
                url,
                platform: "spotify",
                name: "Old name",
                image_url: None,
            },
            20,
        )
        .await
        .unwrap()
    {
        crate::store::SaveOutcome::Saved { id, .. } => id.to_string(),
        crate::store::SaveOutcome::Full => panic!("not saved"),
    };
    let mut song = crate::store::test_util::track("dQw4w9WgXcQ");
    song.title = "Never Gonna Give You Up".into();
    song.thumbnail = Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg".into());
    let cached = crate::sources::CachedList {
        name: Some("Top Hits".into()),
        image: Some("https://i.scdn.co/image/cover".into()),
        tracks: vec![song, crate::store::test_util::track("aaaaaaaaaaa")],
        fetched_at: 1_700_000_000,
    };
    store
        .cache_put(
            crate::sources::lists::CACHE_NS,
            url,
            &cached,
            crate::sources::lists::LIST_TTL,
        )
        .await
        .unwrap();
    (id, url.to_owned())
}

#[tokio::test]
async fn saved_playlists_routes() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (boss, _) = h.login("boss", None).await;
    let (id, url) = seed_playlist(&h, ANA).await;

    let (s, v) = h.call("GET", "/api/playlists", Some(&ana), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["max"].clone(), v["saving"].clone(), v["full"].clone()), (json!(20), json!(true), json!(false)));
    assert_eq!(v["playlists"][0]["id"], id.as_str());
    assert_eq!(v["playlists"][0]["platform"], "spotify");
    assert_eq!(v["playlists"][0]["url"], url.as_str());
    let (_, v) = h.call("GET", "/api/playlists", Some(&boss), None).await;
    assert_eq!(v["playlists"], json!([]), "per user");

    // Songs from the shared cache; the saved name and cover follow the platform.
    let tracks = format!("/api/playlists/{id}/tracks");
    let (s, v) = h.call("GET", &tracks, Some(&ana), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["refreshed"], false);
    assert_eq!(v["fetchedAt"], 1_700_000_000);
    assert_eq!(v["tracks"].as_array().unwrap().len(), 2);
    assert_eq!(v["tracks"][0]["title"], "Never Gonna Give You Up");
    assert!(v["tracks"][0]["artworkUrl"].as_str().unwrap().starts_with("/backend/img?url="));
    assert_eq!(v["playlist"]["name"], "Top Hits");
    assert!(v["playlist"]["imageUrl"].as_str().unwrap().contains("i.scdn.co"));
    let (s, v) = h.call("GET", &tracks, Some(&boss), None).await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_found"), "someone else's list");
    let (s, v) = h.call("GET", "/api/playlists/abc/tracks", Some(&ana), None).await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"));

    // Refresh: the first one goes upstream (Spotify is off here, so it fails without network) ...
    let refresh = format!("/api/playlists/{id}/refresh");
    let (s, v) = h.call("POST", &refresh, Some(&ana), None).await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"), "{v}");
    // ... the next ones inside 5 s answer from the cache ...
    for _ in 0..8 {
        let (s, v) = h.call("POST", &refresh, Some(&ana), None).await;
        assert_eq!((s, v["refreshed"].clone()), (StatusCode::OK, json!(false)), "{v}");
        assert_eq!(v["tracks"].as_array().unwrap().len(), 2);
    }
    // ... and the 10th in a row blocks refreshes for 5 minutes.
    let (s, v) = h.call("POST", &refresh, Some(&ana), None).await;
    assert_eq!((s, code(&v)), (StatusCode::TOO_MANY_REQUESTS, "refresh_blocked"));
    assert_eq!(v["error"]["retryAfter"], 300);
    let (s, v) = h.call("POST", &refresh, Some(&ana), None).await;
    assert_eq!(code(&v), "refresh_blocked", "{s}");
    assert!(v["error"]["retryAfter"].as_u64().unwrap() <= 300);
    // Opening the list still works while refreshes are blocked.
    let (s, _) = h.call("GET", &tracks, Some(&ana), None).await;
    assert_eq!(s, StatusCode::OK);

    // Adding goes through the /api/queue/add checks.
    let add = format!("/api/playlists/{id}/add");
    let (s, v) = h.call("POST", &add, Some(&ana), Some(json!({ "channelId": "101" }))).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "not_in_channel"));
    let (_, v) = h
        .call("PUT", "/api/settings", Some(&boss), Some(json!({ "voiceChannels": ["101"] })))
        .await;
    assert_eq!(v["voiceChannels"], json!(["101"]));
    let (s, v) = h.call("POST", &add, Some(&ana), Some(json!({ "channelId": "100" }))).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "channel_not_allowed"));
    h.call("PUT", "/api/settings", Some(&boss), Some(json!({ "voiceChannels": [] })))
        .await;
    // Allowed: the next check is a free bot (none is connected in tests), like /api/queue/add.
    let (s, v) = h.call("POST", &add, Some(&ana), Some(json!({ "channelId": "100" }))).await;
    assert_eq!((s, code(&v)), (StatusCode::SERVICE_UNAVAILABLE, "bot_unavailable"));
    let (s2, v2) = h
        .call("POST", "/api/queue/add", Some(&ana), Some(json!({ "channelId": "100", "query": url })))
        .await;
    assert_eq!((s2, code(&v2)), (s, code(&v)), "same answer as the queue's add box");

    // Pin, remove.
    let (s, _) = h
        .call("POST", &format!("/api/playlists/{id}/pin"), Some(&ana), Some(json!({ "pinned": true })))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, v) = h
        .call("POST", &format!("/api/playlists/{id}/pin"), Some(&boss), Some(json!({ "pinned": true })))
        .await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_found"));
    let (_, v) = h.call("GET", "/api/playlists", Some(&ana), None).await;
    assert_eq!(v["playlists"][0]["pinned"], true);
    let (s, _) = h.call("DELETE", &format!("/api/playlists/{id}"), Some(&ana), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = h.call("DELETE", &format!("/api/playlists/{id}"), Some(&ana), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn saved_playlists_follow_history_and_data_deletion() {
    let h = harness().await;
    let (ana, _) = h.login("ana", None).await;
    seed_playlist(&h, ANA).await;
    h.call("PUT", "/api/me/prefs", Some(&ana), Some(json!({ "historyEnabled": false })))
        .await;
    let (_, v) = h.call("GET", "/api/playlists", Some(&ana), None).await;
    assert_eq!(v["saving"], false, "history off: nothing new is saved");
    assert_eq!(v["playlists"].as_array().unwrap().len(), 1, "saved ones stay until removed");
    let (s, _) = h.call("DELETE", "/api/me/data", Some(&ana), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (ana, _) = h.login("ana", None).await;
    let (_, v) = h.call("GET", "/api/playlists", Some(&ana), None).await;
    assert_eq!(v["playlists"], json!([]), "Delete my data removes saved playlists");
}

// ---- /health ------------------------------------------------------------------------------

/// No grace: a bot that is not connected counts as down right away.
const NO_GRACE: super::health::Limits = super::health::Limits {
    grace: Duration::ZERO,
    max_ping: Duration::from_millis(1000),
};

async fn health_harness(bots: usize, limits: super::health::Limits) -> Harness {
    let h = harness_full(Duration::from_secs(3600), true, bots, limits).await;
    h.state.app.ytdlp_version.set(Some("2026.11.23".into()));
    h
}

fn connect(h: &Harness, index: usize, name: &str) {
    let bots = &h.state.app.bots;
    bots.on_ready(index, 987_654_321 + index as u64, 123_456_789, name, 1, &[]);
    bots.bot(index).unwrap().set_shard_connected(0, true);
}

async fn fresh(h: &Harness, path: &str) -> (StatusCode, Value) {
    h.state.health.invalidate().await;
    h.call("GET", path, None, None).await
}

#[tokio::test]
async fn health_status_and_bot_routes() {
    let h = health_harness(2, NO_GRACE).await;
    // Nothing connected: down.
    let (s, v) = fresh(&h, "/health").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["status"], "down");
    assert_eq!(h.call("GET", "/health/bots/0", None, None).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h.call("GET", "/health/bots/1", None, None).await.0, StatusCode::SERVICE_UNAVAILABLE);
    let (s, v) = h.call("GET", "/health/bots/2", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(code(&v), "not_found");
    assert_eq!(h.call("GET", "/health/bots/main", None, None).await.0, StatusCode::NOT_FOUND);

    // Main bot up, voice bot down: degraded (still 200).
    connect(&h, 0, "Lynx");
    let (s, v) = fresh(&h, "/health").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "degraded");
    let (s, b) = h.call("GET", "/health/bots/0", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["status"], "ok");
    assert_eq!(b["connected"], true);
    let (s, b) = h.call("GET", "/health/bots/1", None, None).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(b["role"], "voice");
    assert_eq!(b["connected"], false);
    assert!(b["downSince"].as_str().unwrap().ends_with('Z'));

    // Everything up: ok.
    connect(&h, 1, "Lynx Voice");
    let (s, v) = fresh(&h, "/health/").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "ok", "{v}");
    assert_eq!(v["bots"][1]["name"], "Lynx Voice");
    assert_eq!(v["bots"][1]["downSince"], Value::Null);

    // yt-dlp unusable: degraded.
    h.state.app.ytdlp_version.set(None);
    let (s, v) = fresh(&h, "/health").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "degraded");
    assert_eq!(v["services"]["ytdlp"]["ok"], false);
    h.state.app.ytdlp_version.set(Some("2026.11.23".into()));

    // Main bot loses its gateway: down again.
    h.state.app.bots.bot(0).unwrap().set_shard_connected(0, false);
    let (s, v) = fresh(&h, "/health").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["status"], "down");

    // Database gone: down even with every bot connected.
    connect(&h, 0, "Lynx");
    assert_eq!(fresh(&h, "/health").await.1["status"], "ok");
    h.state.app.store.close().await;
    let (s, v) = fresh(&h, "/health").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["services"]["database"]["ok"], false);
}

#[tokio::test]
async fn health_rides_out_reconnects_within_the_grace() {
    let h = health_harness(2, super::health::LIMITS).await;
    // Just started, nobody connected yet: within the grace, so not down.
    let (s, v) = fresh(&h, "/health").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "ok");
    assert_eq!(v["bots"][0]["connected"], false);
    assert!(v["bots"][0]["downSince"].is_string());
    assert_eq!(h.call("GET", "/health/bots/1", None, None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn health_json_shape_has_no_ids_or_secrets() {
    let h = health_harness(2, NO_GRACE).await;
    connect(&h, 0, "Lynx");
    connect(&h, 1, "Lynx Voice");
    h.state.app.bots.set_member(555_555_555, 0, true);
    let req = Request::builder().uri("/health").body(Body::empty()).unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["cache-control"], "no-store");
    assert_eq!(res.headers()["x-robots-tag"], "noindex");
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    for secret in ["987654321", "123456789", "555555555", "token", "/tmp", "lynx-api-test"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    let v: Value = serde_json::from_str(&text).unwrap();
    let keys = |v: &Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(
        keys(&v),
        ["bots", "checkedAt", "commit", "players", "services", "status", "uptimeSecs", "version"]
    );
    assert_eq!(
        keys(&v["bots"][0]),
        [
            "connected",
            "downSince",
            "gatewayPingMs",
            "guilds",
            "index",
            "name",
            "role",
            "shards",
            "status",
            "voiceSessions"
        ]
    );
    assert_eq!(v["bots"][0]["role"], "main");
    assert_eq!(v["bots"][1]["role"], "voice");
    assert_eq!(v["bots"][0]["guilds"], 1);
    assert_eq!(v["bots"][0]["shards"], json!({ "total": 1, "connected": 1 }));
    assert_eq!(v["services"]["database"]["ok"], true);
    assert!(v["services"]["database"]["latencyMs"].is_u64());
    assert_eq!(v["services"]["ytdlp"], json!({ "ok": true, "version": "2026.11.23" }));
    assert_eq!(v["services"]["ollama"], Value::Null, "Ollama is not configured in tests");
    assert_eq!(v["players"], json!({ "active": 0, "playing": 0 }));
    assert!(v["checkedAt"].as_str().unwrap().ends_with('Z'));
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn health_report_is_cached() {
    let h = health_harness(1, NO_GRACE).await;
    connect(&h, 0, "Lynx");
    assert_eq!(fresh(&h, "/health").await.0, StatusCode::OK);
    // The bot drops, but the cached report is served for a few seconds.
    h.state.app.bots.bot(0).unwrap().set_shard_connected(0, false);
    let (s, v) = h.call("GET", "/health", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "ok");
    assert_eq!(h.call("GET", "/health/bots/0", None, None).await.0, StatusCode::OK);
    // Once it expires the new state shows.
    assert_eq!(fresh(&h, "/health").await.0, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn ai_radio_starts_are_rate_limited_per_user() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let start = json!({ "channelId": "100" });
    for _ in 0..5 {
        let (s, v) = h.call("POST", "/api/ai/start", Some(&ana), Some(start.clone())).await;
        assert_ne!(s, StatusCode::TOO_MANY_REQUESTS, "{v}");
    }
    let (s, v) = h.call("POST", "/api/ai/start", Some(&ana), Some(start)).await;
    assert_eq!((s, code(&v)), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    // Shared with the Discord `/ai` (same limiter).
    assert!(!h.state.app.limits.ai.allow(ANA));
    assert!(h.state.app.limits.ai.allow(BOSS));
}

#[tokio::test]
async fn skip_takes_an_optional_expected_uid() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (s, v) = h
        .call(
            "POST",
            "/api/player/skip",
            Some(&ana),
            Some(json!({ "channelId": "100", "expectedUid": "nope" })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::BAD_REQUEST, "invalid"));
    // A valid one reaches the player check (nothing plays here).
    let uid = uuid::Uuid::new_v4().to_string();
    let (s, _) = h
        .call(
            "POST",
            "/api/player/skip",
            Some(&ana),
            Some(json!({ "channelId": "100", "expectedUid": uid })),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn seek_follows_the_control_rule() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (boss, _) = h.login("boss", None).await;
    detached_player(&h, VOICE, &["a", "b"]);
    let seek = json!({ "channelId": "100", "positionMs": 1000 });
    // Everyone mode: the listener reaches the player (nothing is playing yet: the queue waits).
    let (s, v) = h.call("POST", "/api/player/seek", Some(&ana), Some(seek.clone())).await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_found"), "{v}");
    // DJ mode: refused before the player is asked, like pause or skip.
    h.call("PUT", "/api/settings", Some(&boss), Some(json!({ "controlMode": "dj" })))
        .await;
    let (s, v) = h.call("POST", "/api/player/seek", Some(&ana), Some(seek.clone())).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "no_permission"), "{v}");
    let (s, v) = h.call("POST", "/api/player/pause", Some(&ana), Some(json!({ "channelId": "100" }))).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "no_permission"), "{v}");
    // Out of the channel: refused too.
    h.fake.set_voice(ANA, None);
    let (s, v) = h.call("POST", "/api/player/seek", Some(&ana), Some(seek)).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "not_in_channel"), "{v}");
}

#[tokio::test]
async fn add_next_needs_control_in_dj_mode() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    h.fake.set_voice(BOSS, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (boss, _) = h.login("boss", Some(VOICE)).await;
    // Lynx is limited to another channel, so a request that passes the permission rule stops at
    // the channel check (before anything is resolved).
    let (s, v) = h
        .call(
            "PUT",
            "/api/settings",
            Some(&boss),
            Some(json!({ "voiceChannels": ["101"], "controlMode": "dj" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let add = |position: Option<&str>| {
        let mut b = json!({ "channelId": "100", "query": "x" });
        if let Some(p) = position {
            b["position"] = json!(p);
        }
        b
    };
    // A listener may add at the end (default or explicit), not next.
    for body in [add(None), add(Some("end"))] {
        let (s, v) = h.call("POST", "/api/queue/add", Some(&ana), Some(body)).await;
        assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "channel_not_allowed"), "{v}");
    }
    let (s, v) = h.call("POST", "/api/queue/add", Some(&ana), Some(add(Some("next")))).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "no_permission"), "{v}");
    // Everyone mode: the listener may add next too.
    let (s, v) = h.call("PUT", "/api/settings", Some(&boss), Some(json!({ "controlMode": "everyone" }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = h.call("POST", "/api/queue/add", Some(&ana), Some(add(Some("next")))).await;
    assert_eq!((s, code(&v)), (StatusCode::FORBIDDEN, "channel_not_allowed"), "{v}");
    // Anything else is a bad request.
    let (s, _) = h.call("POST", "/api/queue/add", Some(&ana), Some(add(Some("middle")))).await;
    assert_eq!(s.as_u16() / 100, 4);
    assert_ne!(s, StatusCode::FORBIDDEN);
}

/// The next socket message of `kind` (others, like `state` and `position`, are skipped).
async fn next_of(socket: &mut TestSocket, kind: &str) -> Value {
    for _ in 0..50 {
        let v = next(socket).await;
        if v["type"] == kind {
            return v;
        }
    }
    panic!("no {kind} message");
}

async fn send_json(socket: &mut TestSocket, v: Value) {
    use futures_util::SinkExt;
    socket
        .send(tokio_tungstenite::tungstenite::Message::text(v.to_string()))
        .await
        .unwrap();
}

/// A signed-in socket past its first `channel` / `state` (/ `jam`) / `permissions` messages; the state.
async fn ws_ready(addr: std::net::SocketAddr, session: &str) -> (TestSocket, Value) {
    let mut socket = ws_open(addr, session).await;
    assert_eq!(next(&mut socket).await["type"], "channel");
    let state = next(&mut socket).await;
    assert_eq!(state["type"], "state");
    // A jam message may come first.
    next_of(&mut socket, "permissions").await;
    (socket, state)
}

#[tokio::test]
async fn seek_bar_hold_locks_the_others_until_released() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    h.fake.set_voice(BOSS, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (boss, _) = h.login("boss", Some(VOICE)).await;
    detached_player(&h, VOICE, &["a", "b"]);
    let addr = serve(&h).await;
    let (mut a, state) = ws_ready(addr, &ana).await;
    assert_eq!(state["state"]["seekHold"], json!({ "holder": null, "until": null }));
    let (mut b, _) = ws_ready(addr, &boss).await;
    let seek = json!({ "channelId": "100", "positionMs": 1000 });

    // Ana starts dragging: everyone in the channel sees her hold, with its end time.
    send_json(&mut a, json!({ "type": "seek_hold" })).await;
    for s in [&mut a, &mut b] {
        let v = next_of(s, "seek_hold").await;
        assert_eq!(v["holder"], json!({ "id": ANA.to_string(), "name": "ANA" }), "{v}");
        assert!(v["until"].as_i64().unwrap() > 0);
    }
    // A panel opened now sees it in the state.
    let (_, v) = h.call("GET", "/api/player?channelId=100", Some(&boss), None).await;
    assert_eq!(v["seekHold"]["holder"]["id"], ANA.to_string());
    // The other one can not take it, nor seek.
    send_json(&mut b, json!({ "type": "seek_hold" })).await;
    let v = next_of(&mut b, "seek_hold").await;
    assert_eq!((v["denied"].clone(), v["holder"]["id"].clone()), (json!(true), json!(ANA.to_string())));
    let (s, v) = h.call("POST", "/api/player/seek", Some(&boss), Some(seek.clone())).await;
    assert_eq!((s, code(&v)), (StatusCode::CONFLICT, "seek_held"), "{v}");
    // The holder's own seek on release is accepted (nothing plays in this test channel, so the
    // player answers not found) and frees the bar for everyone.
    let (s, v) = h.call("POST", "/api/player/seek", Some(&ana), Some(seek.clone())).await;
    assert_eq!((s, code(&v)), (StatusCode::NOT_FOUND, "not_found"), "{v}");
    for s in [&mut a, &mut b] {
        assert_eq!(next_of(s, "seek_hold").await["holder"], Value::Null);
    }
    let (s, v) = h.call("POST", "/api/player/seek", Some(&boss), Some(seek.clone())).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    // seek_release frees it too.
    send_json(&mut b, json!({ "type": "seek_hold" })).await;
    assert_eq!(next_of(&mut a, "seek_hold").await["holder"]["id"], BOSS.to_string());
    send_json(&mut b, json!({ "type": "seek_release" })).await;
    assert_eq!(next_of(&mut a, "seek_hold").await["holder"], Value::Null);

    // Closing the holder's socket frees it.
    send_json(&mut b, json!({ "type": "seek_hold" })).await;
    assert_eq!(next_of(&mut a, "seek_hold").await["holder"]["id"], BOSS.to_string());
    drop(b);
    assert_eq!(next_of(&mut a, "seek_hold").await["holder"], Value::Null);

    // The player stopping frees it.
    send_json(&mut a, json!({ "type": "seek_hold" })).await;
    assert_eq!(next_of(&mut a, "seek_hold").await["holder"]["id"], ANA.to_string());
    let (s, _) = h
        .call("POST", "/api/player/stop", Some(&boss), Some(json!({ "channelId": "100" })))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(next_of(&mut a, "seek_hold").await["holder"], Value::Null);
}

#[tokio::test]
async fn seek_bar_hold_needs_the_control_rule() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (boss, _) = h.login("boss", None).await;
    detached_player(&h, VOICE, &["a"]);
    h.call("PUT", "/api/settings", Some(&boss), Some(json!({ "controlMode": "dj" })))
        .await;
    let addr = serve(&h).await;
    let (mut a, _) = ws_ready(addr, &ana).await;
    send_json(&mut a, json!({ "type": "seek_hold" })).await;
    let v = next_of(&mut a, "seek_hold").await;
    assert_eq!((v["denied"].clone(), v["holder"].clone()), (json!(true), Value::Null));
}

#[tokio::test]
async fn seek_bar_hold_times_out_and_the_holder_waits() {
    let max = Duration::from_millis(300);
    let h = harness_opts(Duration::from_secs(3600), true, 1, super::health::LIMITS, max).await;
    h.fake.set_voice(ANA, Some(VOICE));
    h.fake.set_voice(BOSS, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (boss, _) = h.login("boss", Some(VOICE)).await;
    detached_player(&h, VOICE, &["a"]);
    let addr = serve(&h).await;
    let (mut a, _) = ws_ready(addr, &ana).await;
    let (mut b, _) = ws_ready(addr, &boss).await;
    let started = std::time::Instant::now();
    send_json(&mut a, json!({ "type": "seek_hold" })).await;
    assert_eq!(next_of(&mut a, "seek_hold").await["holder"]["id"], ANA.to_string());
    // The pointer stays down: the server ends the hold at the limit and tells everyone.
    assert_eq!(next_of(&mut a, "seek_hold").await["holder"], Value::Null);
    assert!(started.elapsed() >= max - Duration::from_millis(50), "{:?}", started.elapsed());
    assert_eq!(next_of(&mut b, "seek_hold").await["holder"]["id"], ANA.to_string());
    assert_eq!(next_of(&mut b, "seek_hold").await["holder"], Value::Null);
    // Asking again at once is refused (cooldown); the other one may take it.
    send_json(&mut a, json!({ "type": "seek_hold" })).await;
    let v = next_of(&mut a, "seek_hold").await;
    assert_eq!((v["denied"].clone(), v["holder"].clone()), (json!(true), Value::Null));
    send_json(&mut b, json!({ "type": "seek_hold" })).await;
    assert_eq!(next_of(&mut b, "seek_hold").await["holder"]["id"], BOSS.to_string());
}

#[tokio::test]
async fn seek_bar_hold_is_shared_by_a_jam_and_ends_with_the_track() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    h.fake.set_voice(BOSS, Some(LOUNGE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let (boss, _) = h.login("boss", Some(LOUNGE)).await;
    detached_player(&h, VOICE, &["a", "b", "c"]);
    detached_player(&h, LOUNGE, &["x"]);
    let (s, v) = h
        .call("POST", "/api/jam/create", Some(&ana), Some(json!({ "channelId": "100" })))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let id = v["jamId"].as_str().unwrap().to_owned();
    let (s, v) = h
        .call(
            "POST",
            "/api/jam/join",
            Some(&boss),
            Some(json!({ "channelId": "101", "jamId": id })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    eventually(&h, "/api/player?channelId=101", &boss, |v| {
        v["jam"]["members"].as_array().is_some_and(|m| m.len() == 2)
    })
    .await;
    // Play: the shared queue starts its first track in every member channel.
    let (s, v) = h
        .call("POST", "/api/player/resume", Some(&ana), Some(json!({ "channelId": "100" })))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    eventually(&h, "/api/player?channelId=101", &boss, |v| v["current"]["title"] == "Song a").await;
    let addr = serve(&h).await;
    let (mut a, _) = ws_ready(addr, &ana).await;
    let (mut b, _) = ws_ready(addr, &boss).await;

    // A hold in the host channel locks the bar in the member channel too.
    send_json(&mut a, json!({ "type": "seek_hold" })).await;
    assert_eq!(next_of(&mut b, "seek_hold").await["holder"]["id"], ANA.to_string());
    let (s, v) = h
        .call(
            "POST",
            "/api/player/seek",
            Some(&boss),
            Some(json!({ "channelId": "101", "positionMs": 1000 })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::CONFLICT, "seek_held"), "{v}");
    let (_, v) = h.call("GET", "/api/player?channelId=101", Some(&boss), None).await;
    assert_eq!(v["seekHold"]["holder"]["id"], ANA.to_string());

    // A new shared track ends it.
    let (s, v) = h
        .call("POST", "/api/player/skip", Some(&boss), Some(json!({ "channelId": "101" })))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    assert_eq!(next_of(&mut b, "seek_hold").await["holder"], Value::Null);
    assert_eq!(next_of(&mut a, "seek_hold").await["holder"]["id"], ANA.to_string());
    assert_eq!(next_of(&mut a, "seek_hold").await["holder"], Value::Null);
}
