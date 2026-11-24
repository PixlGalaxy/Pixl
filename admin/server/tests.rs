//! Admin listener tests: in-memory store, no network.

use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    routing::get,
    Router,
};
use serde_json::{json, Value};
use serenity::{cache::Cache, http::Http};
use songbird::Songbird;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::{auth::Credentials, harden, router, AdminConfig, AdminDeps, AdminState, LOGIN_PER_IP};
use crate::{
    ai::AiEngine,
    discord::registry::{BotInfo, BotRegistry},
    i18n::I18n,
    notices::NoticeBoard,
    player::{PlayerDeps, PlayerManager},
    procs::ProcessRegistry,
    sources::Sources,
    store::{now_secs, SessionWriter, Store},
    AppState,
};

const USER: &str = "operator";
const PASSWORD: &str = "correct horse battery staple";

async fn app_state() -> AppState {
    let dir = std::env::temp_dir().join(format!("lynx-admin-test-{}", uuid::Uuid::new_v4()));
    let config = Arc::new(crate::ai::test_util::config(dir));
    let store = Store::open_in_memory().await.unwrap();
    let i18n = Arc::new(I18n::load("en").unwrap());
    let procs = ProcessRegistry::new();
    let sources = Sources::new(&config, store.clone(), procs.clone()).unwrap();
    let (sessions, _task) = SessionWriter::spawn(store.clone());
    let bots = Arc::new(BotRegistry::new(
        (0..2)
            .map(|i| {
                Arc::new(BotInfo::new(
                    i,
                    Arc::new(Http::new("x")),
                    Arc::new(Cache::new()),
                    Songbird::serenity(),
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
    AppState {
        config,
        store,
        i18n,
        procs,
        sources,
        bots,
        players,
        ai,
        ytdlp_version: Default::default(),
        notices: NoticeBoard::default(),
        limits: Default::default(),
        shutdown: CancellationToken::new(),
    }
}

fn cfg(user: Option<&str>, password: Option<&str>) -> AdminConfig {
    AdminConfig {
        credentials: Credentials::from_parts(user.map(String::from), password.map(String::from), None),
        host: "127.0.0.1".into(),
        port: 0,
        web_dir: None,
        cookie_secure: None,
    }
}

fn deps(app: AppState) -> AdminDeps {
    AdminDeps {
        app,
        main: tokio::runtime::Handle::current(),
        snapshot: tokio::sync::watch::channel(None).1,
    }
}

struct H {
    app: Router,
    state: AppState,
}

async fn harness() -> H {
    let state = app_state().await;
    let admin = AdminState::new(deps(state.clone()), &cfg(Some(USER), Some(PASSWORD))).unwrap();
    H {
        app: router(admin, None),
        state,
    }
}

struct Res {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Value,
}

impl H {
    async fn send(
        &self,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: Option<Value>,
        extra: &[(&str, &str)],
    ) -> Res {
        let mut req = Request::builder().method(method).uri(path);
        if !extra.iter().any(|(k, _)| *k == "x-real-ip") {
            req = req.header("x-real-ip", "198.51.100.7");
        }
        if method != "GET" {
            req = req.header("x-lynx-admin", "1");
        }
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, format!("lynx_admin={c}"));
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
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
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Res { status, headers, body }
    }

    async fn login(&self) -> String {
        let r = self
            .send(
                "POST",
                "/api/login",
                None,
                Some(json!({ "username": USER, "password": PASSWORD })),
                &[],
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        let cookie = r.headers[header::SET_COOKIE].to_str().unwrap().to_owned();
        cookie
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("lynx_admin=")
            .unwrap()
            .to_owned()
    }
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn admin_is_off_without_username_and_password() {
    let state = app_state().await;
    assert!(AdminState::new(deps(state.clone()), &cfg(None, None)).is_none());
    assert!(
        AdminState::new(deps(state.clone()), &cfg(None, Some(PASSWORD))).is_none(),
        "no username"
    );
    assert!(
        AdminState::new(deps(state.clone()), &cfg(Some(USER), None)).is_none(),
        "no password"
    );
    assert!(
        AdminState::new(deps(state), &cfg(Some(USER), Some("short"))).is_none(),
        "too short"
    );
    assert!(!cfg(None, None).enabled());
    assert!(cfg(Some(USER), Some(PASSWORD)).enabled());
    assert!(
        super::start(app_state().await, cfg(None, None)).is_none(),
        "nothing starts when off"
    );
    let debug = format!("{:?}", cfg(Some(USER), Some(PASSWORD)));
    assert!(
        !debug.contains(PASSWORD) && !debug.contains(USER),
        "credentials never show in logs: {debug}"
    );
}

#[tokio::test]
async fn login_cookie_and_logout() {
    let h = harness().await;
    let r = h.send("GET", "/api/session", None, None, &[]).await;
    assert_eq!(r.body, json!({ "enabled": true, "authenticated": false }));
    assert_eq!(r.headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(r.headers[header::X_FRAME_OPTIONS], "DENY");
    assert!(r.headers[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .contains("frame-ancestors 'none'"));

    for (u, p) in [
        (USER, "wrong"),
        ("admin", PASSWORD),
        ("", ""),
        (USER, &format!("{PASSWORD} ")[..]),
    ] {
        let r = h
            .send(
                "POST",
                "/api/login",
                None,
                Some(json!({ "username": u, "password": p })),
                &[("x-real-ip", "198.51.100.50")],
            )
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{u}/{p}");
        assert_eq!(code(&r.body), "wrong_credentials");
        assert!(r.headers.get(header::SET_COOKIE).is_none());
    }
    let r = h
        .send("POST", "/api/login", None, Some(json!({ "password": PASSWORD })), &[])
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "the username is required");

    let r = h
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "username": USER, "password": PASSWORD })),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let cookie = r.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.starts_with("lynx_admin="), "{cookie}");
    assert!(cookie.contains("; HttpOnly"), "{cookie}");
    assert!(cookie.contains("; SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("; Path=/"), "{cookie}");
    assert!(
        cookie.contains(&format!("; Max-Age={}", super::SESSION_TTL.as_secs())),
        "{cookie}"
    );
    assert!(!cookie.contains("Secure"), "plain http (dev): no Secure flag");
    let token = cookie
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("lynx_admin=")
        .unwrap()
        .to_owned();
    assert_eq!(token.len(), 64);

    let r = h
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "username": USER, "password": PASSWORD })),
            &[("x-forwarded-proto", "https")],
        )
        .await;
    assert!(
        r.headers[header::SET_COOKIE].to_str().unwrap().ends_with("; Secure"),
        "Secure behind HTTPS"
    );

    let r = h.send("GET", "/api/session", Some(&token), None, &[]).await;
    assert_eq!(r.body["authenticated"], true);
    assert_eq!(
        h.send("GET", "/api/tweaks", Some(&token), None, &[]).await.status,
        StatusCode::OK
    );

    let r = h.send("POST", "/api/logout", Some(&token), None, &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers[header::SET_COOKIE].to_str().unwrap().contains("Max-Age=0"));
    let r = h.send("GET", "/api/tweaks", Some(&token), None, &[]).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "the session is gone after logout");
}

#[tokio::test]
async fn login_is_rate_limited_per_ip() {
    let h = harness().await;
    for _ in 0..LOGIN_PER_IP {
        let r = h
            .send(
                "POST",
                "/api/login",
                None,
                Some(json!({ "username": USER, "password": "nope" })),
                &[],
            )
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    // Even the right credentials are refused while over the limit.
    let r = h
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "username": USER, "password": PASSWORD })),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(r.headers.get(header::SET_COOKIE).is_none());
    // Another address still can.
    let r = h
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "username": USER, "password": PASSWORD })),
            &[("x-real-ip", "198.51.100.8")],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn everything_but_the_session_check_needs_a_login() {
    let h = harness().await;
    for (m, p) in [
        ("GET", "/api/overview"),
        ("GET", "/api/activity"),
        ("GET", "/api/logs"),
        ("GET", "/api/logs/stream"),
        ("GET", "/api/notices"),
        ("POST", "/api/notices"),
        ("PUT", "/api/notices/1"),
        ("DELETE", "/api/notices/1"),
        ("GET", "/api/tweaks"),
        ("PUT", "/api/tweaks"),
        ("POST", "/api/actions/ytdlp-update"),
        ("POST", "/api/actions/clear-stream-cache"),
        ("POST", "/api/actions/clear-lookup-cache"),
    ] {
        let body = (m == "POST" || m == "PUT").then(|| json!({}));
        let r = h.send(m, p, Some(&"0".repeat(64)), body.clone(), &[]).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{m} {p}");
        let r = h.send(m, p, None, body, &[]).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{m} {p}");
    }
    let r = h.send("GET", "/api/nope", None, None, &[]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mutating_requests_need_the_csrf_header_and_same_origin() {
    let h = harness().await;
    let token = h.login().await;
    let req = |headers: &[(&str, &str)]| {
        let mut b = Request::builder()
            .method("POST")
            .uri("/api/actions/clear-stream-cache")
            .header(header::COOKIE, format!("lynx_admin={token}"));
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    };
    let status = |r: Request<Body>| {
        let app = h.app.clone();
        async move { app.oneshot(r).await.unwrap().status() }
    };
    assert_eq!(status(req(&[])).await, StatusCode::FORBIDDEN, "no admin header");
    assert_eq!(
        status(req(&[
            ("x-lynx-admin", "1"),
            ("host", "lynx.lan:81"),
            ("origin", "https://evil.example")
        ]))
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(req(&[("x-lynx-admin", "1"), ("sec-fetch-site", "cross-site")])).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(req(&[
            ("x-lynx-admin", "1"),
            ("host", "lynx.lan:81"),
            ("origin", "http://lynx.lan:81")
        ]))
        .await,
        StatusCode::OK
    );
    let r = Request::builder()
        .method("POST")
        .uri("/api/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "username": USER, "password": PASSWORD }).to_string(),
        ))
        .unwrap();
    assert_eq!(status(r).await, StatusCode::FORBIDDEN, "sign-in is guarded too");
}

#[tokio::test]
async fn notices_crud_updates_the_board() {
    let h = harness().await;
    let token = h.login().await;
    let now = now_secs();
    let start = (now + 3600) * 1000;
    let rx = h.state.notices.subscribe();

    let r = h
        .send(
            "POST",
            "/api/notices",
            Some(&token),
            Some(json!({ "kind": "maintenance", "title": "Upgrade", "message": "New host", "startsAt": start, "durationMinutes": 10 })),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let list = r.body["notices"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    let id = list[0]["id"].as_i64().unwrap();
    assert_eq!(list[0]["endsAt"], start + 600_000);
    assert_eq!(list[0]["phase"], "upcoming", "show from defaults to now");
    assert!(rx.has_changed().unwrap());
    let (n, _) = h.state.notices.current(now).unwrap();
    assert_eq!(n.title, "Upgrade");
    assert_eq!(h.state.store.notices().await.unwrap().len(), 1, "saved in SQLite");

    for bad in [
        json!({ "kind": "party", "title": "x", "startsAt": start, "durationMinutes": 10 }),
        json!({ "kind": "info", "startsAt": start, "durationMinutes": 10 }),
        json!({ "kind": "info", "title": "x", "startsAt": start }),
        json!({ "kind": "info", "title": "x", "startsAt": start, "durationMinutes": 0 }),
        json!({ "kind": "info", "title": "x", "startsAt": start, "durationMinutes": 20000 }),
        json!({ "kind": "info", "title": "x", "startsAt": start, "endsAt": start - 1 }),
        json!({ "kind": "info", "title": "x".repeat(101), "startsAt": start, "durationMinutes": 5 }),
        json!({ "kind": "info", "title": "x", "message": "y".repeat(501), "startsAt": start, "durationMinutes": 5 }),
        json!({ "kind": "info", "title": "x", "startsAt": 5, "durationMinutes": 5 }),
        json!({ "kind": "info", "title": "x", "startsAt": (now - 7200) * 1000, "durationMinutes": 5 }),
        json!({ "kind": "info", "title": "x", "startsAt": start, "durationMinutes": 5, "showFrom": start + 600_000 }),
        json!({ "kind": "info", "title": "x", "startsAt": start, "durationMinutes": 5, "extra": 1 }),
    ] {
        let r = h
            .send("POST", "/api/notices", Some(&token), Some(bad.clone()), &[])
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}: {}", r.body);
    }

    let r = h
        .send(
            "PUT",
            &format!("/api/notices/{id}"),
            Some(&token),
            Some(json!({ "kind": "warning", "title": "Later", "startsAt": start, "endsAt": start + 1_800_000, "showFrom": start - 60_000 })),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["notices"][0]["kind"], "warning");
    assert_eq!(r.body["notices"][0]["phase"], Value::Null, "not announced yet");
    assert!(h.state.notices.current(now).is_none());
    assert_eq!(h.state.notices.public_json(now)["notices"], json!([]));
    let r = h
        .send(
            "PUT",
            "/api/notices/999",
            Some(&token),
            Some(json!({ "kind": "info", "title": "x", "startsAt": start, "durationMinutes": 5 })),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let r = h
        .send("DELETE", &format!("/api/notices/{id}"), Some(&token), None, &[])
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["notices"], json!([]));
    assert!(h.state.notices.notices().is_empty());
    let r = h
        .send("DELETE", &format!("/api/notices/{id}"), Some(&token), None, &[])
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = h.send("DELETE", "/api/notices/abc", Some(&token), None, &[]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn tweaks_set_flags_and_clear_caches() {
    let h = harness().await;
    let token = h.login().await;
    let r = h.send("GET", "/api/tweaks", Some(&token), None, &[]).await;
    assert_eq!(
        r.body["flags"],
        json!({ "maintenanceMode": false, "ytdlpUpdatesPaused": false })
    );

    let r = h
        .send(
            "PUT",
            "/api/tweaks",
            Some(&token),
            Some(json!({ "maintenanceMode": true })),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["flags"]["maintenanceMode"], true);
    assert!(h.state.notices.maintenance_mode());
    assert!(
        h.state.store.admin_flags().await.unwrap().maintenance_mode,
        "kept across restarts"
    );

    let r = h
        .send(
            "PUT",
            "/api/tweaks",
            Some(&token),
            Some(json!({ "ytdlpUpdatesPaused": true })),
            &[],
        )
        .await;
    assert_eq!(
        r.body["flags"],
        json!({ "maintenanceMode": true, "ytdlpUpdatesPaused": true })
    );
    assert_eq!(r.body["ytdlp"]["updatesPaused"], true);
    assert!(h.state.sources.ytdlp().updates_paused());
    let r = h
        .send(
            "PUT",
            "/api/tweaks",
            Some(&token),
            Some(json!({ "command": "rm" })),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "unknown fields are refused");

    h.state
        .store
        .cache_put("search", "k", &1, Duration::from_secs(60))
        .await
        .unwrap();
    let r = h
        .send("POST", "/api/actions/clear-lookup-cache", Some(&token), None, &[])
        .await;
    assert_eq!(r.body["removed"], 1);
    let r = h
        .send("POST", "/api/actions/clear-stream-cache", Some(&token), None, &[])
        .await;
    assert_eq!(r.body["removed"], 0);
}

#[tokio::test]
async fn overview_activity_and_logs() {
    let h = harness().await;
    let token = h.login().await;
    let r = h.send("GET", "/api/overview", Some(&token), None, &[]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body["health"]["status"].is_string());
    assert_eq!(r.body["health"]["bots"].as_array().unwrap().len(), 2);
    assert_eq!(r.body["labels"], json!(["Main", "Voice A"]));
    assert_eq!(r.body["totals"]["playersActive"], 0);
    assert!(r.body["totals"]["listeners"].is_number());
    assert_eq!(r.body["health"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(r.body["notices"], json!({ "total": 0, "visible": 0 }));
    assert!(
        !r.body.to_string().to_lowercase().contains("node"),
        "never 'node' in what the UI shows"
    );

    crate::stats::command("play");
    let r = h.send("GET", "/api/activity", Some(&token), None, &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body["hour"]["totals"]["commands"].as_u64().unwrap() >= 1);
    assert_eq!(r.body["hour"]["series"]["commands"].as_array().unwrap().len(), 60);
    assert_eq!(r.body["day"]["series"]["commands"].as_array().unwrap().len(), 96);

    let marker = format!("admin log test {}", uuid::Uuid::new_v4());
    crate::logs::global().push("info", "lynx::test", &marker);
    let r = h.send("GET", "/api/logs", Some(&token), None, &[]).await;
    assert!(r.body["lines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["text"] == marker.as_str()));
    let latest = r.body["latest"].as_u64().unwrap();
    let r = h
        .send("GET", &format!("/api/logs?after={latest}"), Some(&token), None, &[])
        .await;
    assert!(r.body["lines"]
        .as_array()
        .unwrap()
        .iter()
        .all(|l| l["seq"].as_u64().unwrap() > latest));
}

#[tokio::test]
async fn log_stream_sends_the_backlog_then_live_lines() {
    let h = harness().await;
    let token = h.login().await;
    let marker = format!("stream test {}", uuid::Uuid::new_v4());
    let hub = crate::logs::global();
    hub.push("warn", "lynx::test", &format!("{marker} one"));
    let after = hub.latest_seq() - 1;
    let req = Request::builder()
        .uri(format!("/api/logs/stream?after={after}"))
        .header(header::COOKIE, format!("lynx_admin={token}"))
        .body(Body::empty())
        .unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
    let mut body = res.into_body().into_data_stream();
    use futures_util::StreamExt;
    hub.push("info", "lynx::test", &format!("{marker} two"));
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !(seen.contains(&format!("{marker} one")) && seen.contains(&format!("{marker} two"))) {
        let chunk = tokio::time::timeout_at(deadline, body.next())
            .await
            .expect("stream stalled")
            .unwrap()
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(seen.contains("event: line"));
}

#[tokio::test]
async fn slow_or_panicking_handlers_are_contained() {
    let app = harden(
        Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "late"
                }),
            )
            .route(
                "/boom",
                get(|| async {
                    if true {
                        panic!("admin handler bug");
                    }
                    "never"
                }),
            ),
        Duration::from_millis(100),
    );
    let call = |p: &str| {
        let app = app.clone();
        let req = Request::builder().uri(p).body(Body::empty()).unwrap();
        async move { app.oneshot(req).await.unwrap().status() }
    };
    let started = std::time::Instant::now();
    assert_eq!(call("/slow").await, StatusCode::REQUEST_TIMEOUT, "the timeout fires");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        call("/boom").await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a panic becomes a 500"
    );
    let big = Request::builder()
        .method("POST")
        .uri("/slow")
        .body(Body::from(vec![b'x'; 128 * 1024]))
        .unwrap();
    let _ = app.clone().oneshot(big).await.unwrap();
}

#[tokio::test]
async fn the_bot_keeps_running_when_the_admin_port_is_taken() {
    // Occupy a port, then start the admin panel on it: start() returns, the thread retries in the
    // background, and nothing here blocks or panics.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let app = app_state().await;
    let stop = app.shutdown.clone();
    let mut c = cfg(Some(USER), Some(PASSWORD));
    c.port = port;
    let handle = super::start(app, c).expect("admin thread started");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!handle.is_finished(), "still retrying, not crashed");
    stop.cancel();
    let joined = tokio::task::spawn_blocking(move || handle.join()).await.unwrap();
    assert!(joined.is_ok(), "the admin thread ends cleanly at shutdown");
}

#[tokio::test]
async fn built_ui_is_served_only_when_configured() {
    let dir = std::env::temp_dir().join(format!("lynx-admin-ui-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(dir.join("index.html"), "<!doctype html><title>Lynx admin</title>").unwrap();
    std::fs::write(dir.join("assets/app.js"), "console.log(1)").unwrap();
    let state = app_state().await;
    let app = router(
        AdminState::new(deps(state), &cfg(Some(USER), Some(PASSWORD))).unwrap(),
        Some(dir.clone()),
    );
    let get = |p: &str| {
        let app = app.clone();
        let req = Request::builder().uri(p).body(Body::empty()).unwrap();
        async move {
            let res = app.oneshot(req).await.unwrap();
            let status = res.status();
            let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };
    assert!(get("/").await.1.contains("Lynx admin"));
    assert!(
        get("/notices").await.1.contains("Lynx admin"),
        "client-side routes get the page"
    );
    assert_eq!(get("/assets/app.js").await.1, "console.log(1)");
    assert!(!get("/../Cargo.toml").await.1.contains("[package]"));
    let (s, body) = get("/api/missing").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(body.contains("not_found"), "unknown API paths stay JSON 404s");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn isolated_listener_serves_on_its_own_runtime() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let app = app_state().await;
    let stop = app.shutdown.clone();
    let mut c = cfg(Some(USER), Some(PASSWORD));
    c.port = port;
    let handle = super::start(app, c).expect("admin thread started");
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/api/session");
    let mut body = None;
    for _ in 0..50 {
        if let Ok(r) = client.get(&url).send().await {
            body = Some(r.json::<Value>().await.unwrap());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(body, Some(json!({ "enabled": true, "authenticated": false })));
    // Sign in over a real socket: the peer address (loopback) is seen, the cookie comes back.
    let r = client
        .post(format!("http://127.0.0.1:{port}/api/login"))
        .header("x-lynx-admin", "1")
        .json(&json!({ "username": USER, "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::OK);
    assert!(r.headers().get("set-cookie").is_some());
    stop.cancel();
    let joined = tokio::task::spawn_blocking(move || handle.join()).await.unwrap();
    assert!(joined.is_ok());
}
