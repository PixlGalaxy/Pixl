//! Operator notices on the Activity side: public read-only data, live pushes, maintenance mode,
//! and no admin route on this router.

use axum::http::StatusCode;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{code, harness, next, ANA, VOICE};
use crate::{
    notices::{test_util::notice, Flags, Kind},
    store::now_secs,
};

#[tokio::test]
async fn admin_routes_are_not_on_the_activity_router() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    for token in [None, Some(ana.as_str())] {
        for (m, p) in [
            ("GET", "/api/session"),
            ("POST", "/api/login"),
            ("POST", "/api/logout"),
            ("GET", "/api/overview"),
            ("POST", "/api/notices"),
            ("PUT", "/api/notices/1"),
            ("DELETE", "/api/notices/1"),
            ("GET", "/api/tweaks"),
            ("PUT", "/api/tweaks"),
            ("POST", "/api/actions/ytdlp-update"),
            ("POST", "/api/actions/clear-stream-cache"),
            ("POST", "/api/actions/clear-lookup-cache"),
            ("GET", "/admin"),
            ("GET", "/admin.html"),
            ("GET", "/backend/api/overview"),
        ] {
            let (s, _) = h
                .call(m, p, token, Some(json!({ "password": "x", "maintenanceMode": true })))
                .await;
            assert!(
                s == StatusCode::NOT_FOUND || s == StatusCode::METHOD_NOT_ALLOWED,
                "{m} {p} answered {s} on the Activity router"
            );
        }
    }
    assert!(h.state.app.notices.notices().is_empty());
    assert!(!h.state.app.notices.maintenance_mode());
}

#[tokio::test]
async fn public_notices_are_read_only_and_filtered() {
    let h = harness().await;
    let now = now_secs();
    let mut later = notice(2, Kind::Info, now + 90_000, 10, now + 80_000);
    later.title = "not yet".into();
    h.state
        .app
        .notices
        .set_notices(vec![notice(1, Kind::Maintenance, now + 600, 10, now - 1), later]);

    let (s, v) = h.call("GET", "/api/notices", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["maintenance"], false);
    let list = v["notices"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{v}");
    assert_eq!(list[0]["kind"], "maintenance");
    assert_eq!(list[0]["title"], "Upgrade");
    assert_eq!(list[0]["startsAt"], (now + 600) * 1000);
    assert_eq!(list[0]["endsAt"], (now + 1200) * 1000);
    assert!(
        !v.to_string().contains("not yet"),
        "a notice is not public before its show-from time"
    );

    let (_, v) = h.call("GET", "/api/public", None, None).await;
    assert_eq!(v["notices"][0]["id"], 1);
    assert_eq!(v["maintenance"], false);
}

#[tokio::test]
async fn maintenance_mode_refuses_new_songs_from_the_panel() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let now = now_secs();
    h.state
        .app
        .notices
        .set_notices(vec![notice(1, Kind::Maintenance, now - 60, 30, now - 600)]);
    h.state.app.notices.set_flags(Flags {
        maintenance_mode: true,
        ..Flags::default()
    });
    let (s, v) = h
        .call(
            "POST",
            "/api/queue/add",
            Some(&ana),
            Some(json!({ "channelId": VOICE.to_string(), "query": "never gonna give you up" })),
        )
        .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(code(&v), "maintenance");
    assert!(v["error"]["message"].as_str().unwrap().contains("Upgrade"));
    let (s, v) = h
        .call(
            "POST",
            "/api/ai/start",
            Some(&ana),
            Some(json!({ "channelId": VOICE.to_string() })),
        )
        .await;
    assert_eq!((s, code(&v)), (StatusCode::SERVICE_UNAVAILABLE, "maintenance"));
    let (_, v) = h.call("GET", "/api/public", None, None).await;
    assert_eq!(v["maintenance"], true);

    // Off again: the add goes on to the normal checks (here: no free bot in the test harness).
    h.state.app.notices.set_flags(Flags::default());
    let (_, v) = h
        .call(
            "POST",
            "/api/queue/add",
            Some(&ana),
            Some(json!({ "channelId": VOICE.to_string(), "query": "x" })),
        )
        .await;
    assert_ne!(code(&v), "maintenance");
}

#[tokio::test]
async fn open_panels_get_notice_changes_over_the_socket() {
    let h = harness().await;
    h.fake.set_voice(ANA, Some(VOICE));
    let (ana, _) = h.login("ana", Some(VOICE)).await;
    let stop = CancellationToken::new();
    tokio::spawn(super::super::notices::forward(h.state.clone(), stop.clone()));

    let addr = super::serve(&h).await;
    let mut socket = super::ws_open(addr, &ana).await;
    // The first message means the socket is subscribed to the events.
    next(&mut socket).await;
    let now = now_secs();
    h.state
        .app
        .notices
        .set_notices(vec![notice(7, Kind::Warning, now + 300, 5, now - 1)]);
    let v = loop {
        let v = next(&mut socket).await;
        if v["type"] == "notices" {
            break v;
        }
    };
    assert_eq!(v["notices"][0]["id"], 7);
    assert_eq!(v["notices"][0]["kind"], "warning");
    assert_eq!(v["maintenance"], false);
    stop.cancel();
}
