use std::time::Duration;

use serde_json::json;
use tokio::sync::oneshot;

use super::*;
use crate::{
    ai::Reason,
    perms::Actor,
    store::{test_util::track, ControlMode},
};

fn owner() -> Requester {
    Requester {
        id: 7,
        name: Some("Ana".into()),
        avatar_url: None,
    }
}

fn spec() -> RadioSpec {
    RadioSpec {
        owner: owner(),
        prompt: Some("calm".into()),
        mix: RadioMix::Balanced,
        blend: true,
    }
}

fn radio(kind: JobKind) -> (Radio, watch::Receiver<RadioProgress>) {
    let (job, rx) = Job::new(Uuid::new_v4(), kind, CancellationToken::new());
    (Radio::new(spec(), job), rx)
}

fn pick(id: &str) -> RadioPick {
    RadioPick {
        track: track(id),
        reason: Reason::because_you_like("Night Drive"),
        genres: vec!["rock".into()],
    }
}

#[test]
fn ai_state_json_shape() {
    let (mut r, _) = radio(JobKind::Start);
    let id = r.job.as_ref().unwrap().id;
    r.job.as_mut().unwrap().set_phase(AiPhase::LoadingModel);
    let v = serde_json::to_value(r.snapshot().to_api()).unwrap();
    let started = r.job.as_ref().unwrap().started_at;
    assert_eq!(
        v,
        json!({
            "active": true,
            "ownerId": "7",
            "job": { "id": id.to_string(), "phase": "loading_model", "startedAt": started, "canCancel": true }
        })
    );
    // Every phase string as in plan/api.md.
    let names = [
        (AiPhase::Queued, "queued"),
        (AiPhase::LoadingModel, "loading_model"),
        (AiPhase::ReadingTaste, "reading_taste"),
        (AiPhase::FindingSongs, "finding_songs"),
        (AiPhase::Thinking, "thinking"),
        (AiPhase::Resolving, "resolving"),
        (AiPhase::Ready, "ready"),
        (AiPhase::Failed, "failed"),
        (AiPhase::Cancelled, "cancelled"),
    ];
    for (p, s) in names {
        assert_eq!(serde_json::to_value(p).unwrap(), json!(s));
    }
    // A finished job stays visible but cannot be cancelled.
    r.job_done(id, Ok(0), None);
    let snap = r.snapshot();
    assert_eq!(snap.job.as_ref().unwrap().phase, AiPhase::Failed, "nothing was added");
    assert!(!snap.job.unwrap().can_cancel);
    assert!(!snap.active);
}

#[test]
fn snapshot_api_carries_ai_and_reasons() {
    use crate::player::state::{ApiContext, PlayerSnapshot, Status};
    let entries = pick_entries(vec![pick("a")], &owner());
    assert_eq!(entries[0].requester, owner());
    assert_eq!(
        entries[0].reason,
        Some(json!({ "key": "because_you_like", "params": { "name": "Night Drive" } }))
    );
    let (r, _) = radio(JobKind::Start);
    let snap = PlayerSnapshot {
        guild_id: 1,
        channel_id: 2,
        text_channel_id: None,
        bot_index: 0,
        bot_user_id: 3,
        status: Status::Playing,
        paused_reason: None,
        empty_channel_deadline: None,
        current: Some(entries[0].clone()),
        position_ms: 0,
        position_at: 0,
        upcoming: vec![],
        history: vec![],
        loop_mode: Default::default(),
        volume: 100,
        vote_skip_enabled: false,
        skip_votes: None,
        ai: Some(r.snapshot()),
    };
    let v = serde_json::to_value(snap.to_api(&ApiContext::default())).unwrap();
    assert_eq!(v["ai"]["active"], true);
    assert_eq!(v["ai"]["ownerId"], "7");
    assert_eq!(v["ai"]["job"]["phase"], "queued");
    assert_eq!(v["current"]["reason"]["key"], "because_you_like");
    assert_eq!(v["current"]["reason"]["params"]["name"], "Night Drive");
    assert_eq!(snap.ai_api().unwrap().owner_id, "7");
}

#[test]
fn refill_trigger() {
    let (mut r, _) = radio(JobKind::Start);
    let cur = Some(Uuid::new_v4());
    assert_eq!(r.refill_size(0, 100, cur), None, "the start job is still running");
    let id = r.job.as_ref().unwrap().id;
    r.job.as_mut().unwrap().added(5, false, true);
    r.job_done(id, Ok(0), cur);
    assert_eq!(r.job.as_ref().unwrap().phase, AiPhase::Ready);
    assert_eq!(r.refill_size(REFILL_BELOW, 100, cur), None, "enough upcoming");
    assert_eq!(r.refill_size(REFILL_BELOW - 1, 100, cur), Some(REFILL_BATCH));
    assert_eq!(r.refill_size(0, 4, cur), Some(4), "never past max_queue_length");
    assert_eq!(r.refill_size(2, 2, cur), None, "queue full");

    // One at a time: a running refill blocks the next.
    let (job, _) = Job::new(Uuid::new_v4(), JobKind::Refill, CancellationToken::new());
    let rid = job.id;
    r.job = Some(job);
    assert_eq!(r.refill_size(0, 100, cur), None);

    // A cancelled refill holds refills until the track changes.
    assert!(r.cancel(Some(rid), cur));
    assert!(r.active, "cancel keeps the radio");
    assert_eq!(r.refill_size(0, 100, cur), None);
    assert_eq!(r.refill_size(0, 100, Some(Uuid::new_v4())), Some(REFILL_BATCH));

    // Failed refills turn the radio off after MAX_REFILL_FAILURES.
    for i in 0..MAX_REFILL_FAILURES {
        let (job, _) = Job::new(Uuid::new_v4(), JobKind::Refill, CancellationToken::new());
        let id = job.id;
        r.job = Some(job);
        r.job_done(id, Err(RadioError::NoCandidates), cur);
        assert_eq!(r.active, i + 1 < MAX_REFILL_FAILURES);
    }
    assert_eq!(r.refill_size(0, 100, Some(Uuid::new_v4())), None);

    // Stopped radios never refill.
    let (mut r, _) = radio(JobKind::Start);
    r.stop();
    assert_eq!(r.refill_size(0, 100, cur), None);
}

#[test]
fn successful_refill_resets_failures() {
    let (mut r, _) = radio(JobKind::Start);
    r.failures = 2;
    let (mut job, _) = Job::new(Uuid::new_v4(), JobKind::Refill, CancellationToken::new());
    job.added(3, false, false);
    let id = job.id;
    r.job = Some(job);
    r.job_done(id, Ok(0), None);
    assert_eq!(r.failures, 0);
    assert!(r.active);
}

#[test]
fn owner_dislike_skip_rule() {
    let (mut r, _) = radio(JobKind::Start);
    let playing = Uuid::new_v4();
    assert!(r.owner_dislike_skips(7, true, playing, Some(playing)));
    assert!(!r.owner_dislike_skips(8, true, playing, Some(playing)), "not the owner");
    assert!(!r.owner_dislike_skips(7, false, playing, Some(playing)), "a like");
    assert!(
        !r.owner_dislike_skips(7, true, playing, Some(Uuid::new_v4())),
        "no longer playing"
    );
    r.stop();
    assert!(!r.owner_dislike_skips(7, true, playing, Some(playing)), "radio off");
}

#[test]
fn stop_and_cancel_update_progress() {
    let (mut r, mut progress) = radio(JobKind::Start);
    let id = r.job.as_ref().unwrap().id;
    let token = r.job.as_ref().unwrap().cancel.clone();
    assert!(!r.cancel(Some(Uuid::new_v4()), None), "another job id");
    assert!(r.cancel(Some(id), None));
    assert!(token.is_cancelled());
    assert!(!r.active, "cancelling the start job ends the radio");
    assert_eq!(progress.borrow_and_update().phase, AiPhase::Cancelled);
    assert!(!r.cancel(None, None), "nothing running");
    // A late RadioDone of the cancelled job changes nothing.
    r.job_done(id, Ok(0), None);
    assert_eq!(r.job.as_ref().unwrap().phase, AiPhase::Cancelled);

    let (mut r, progress) = radio(JobKind::Start);
    assert!(r.stop());
    assert!(!r.stop(), "already off");
    assert_eq!(progress.borrow().phase, AiPhase::Cancelled);

    // A failed start that already queued its early pick keeps the radio on.
    let (mut r, progress) = radio(JobKind::Start);
    let id = r.job.as_ref().unwrap().id;
    r.job.as_mut().unwrap().added(1, false, true);
    r.job_done(id, Err(RadioError::Internal("x".into())), None);
    assert!(r.active);
    let p = progress.borrow().clone();
    assert_eq!((p.phase, p.added, p.first_queued), (AiPhase::Failed, 1, true));
    assert_eq!(p.error, Some(RadioError::Internal("x".into())));
}

fn settings(mode: ControlMode) -> GuildSettings {
    GuildSettings {
        control_mode: mode,
        ..GuildSettings::default()
    }
}

#[test]
fn permission_rules() {
    let listener = Actor {
        in_channel: true,
        ..Actor::default()
    };
    let outside = Actor::default();
    let dj = Actor {
        is_dj: true,
        in_channel: true,
        ..Actor::default()
    };
    let manager_away = Actor {
        is_manager: true,
        ..Actor::default()
    };
    let dj_mode = settings(ControlMode::Dj);
    let start = RadioOp::Start { replaces_other: false };
    let replace = RadioOp::Start { replaces_other: true };
    // canAdd starts.
    assert_eq!(radio_permission(&listener, &dj_mode, start, false), Ok(()));
    assert_eq!(
        radio_permission(&outside, &dj_mode, start, false),
        Err(RadioApiError::NotInChannel)
    );
    // Replacing someone else's radio needs canControl.
    assert_eq!(
        radio_permission(&listener, &dj_mode, replace, false),
        Err(RadioApiError::NoPermission)
    );
    assert_eq!(radio_permission(&dj, &dj_mode, replace, false), Ok(()));
    assert_eq!(
        radio_permission(&listener, &settings(ControlMode::Everyone), replace, false),
        Ok(())
    );
    // The owner or canControl stops / cancels.
    let stop = RadioOp::StopOrCancel;
    assert_eq!(radio_permission(&listener, &dj_mode, stop, true), Ok(()));
    assert_eq!(
        radio_permission(&outside, &dj_mode, stop, true),
        Ok(()),
        "owner, even away"
    );
    assert_eq!(
        radio_permission(&listener, &dj_mode, stop, false),
        Err(RadioApiError::NoPermission)
    );
    assert_eq!(radio_permission(&dj, &dj_mode, stop, false), Ok(()));
    assert_eq!(radio_permission(&manager_away, &dj_mode, stop, false), Ok(()));
    assert_eq!(
        radio_permission(&outside, &dj_mode, stop, false),
        Err(RadioApiError::NotInChannel)
    );
}

#[test]
fn error_codes() {
    let cases = [
        (RadioApiError::NoPermission, "no_permission", 403),
        (RadioApiError::HistoryDisabled, "history_disabled", 403),
        (RadioApiError::NotInChannel, "not_in_channel", 403),
        (
            RadioApiError::BotUnavailable(BotUnavailable::CannotJoin),
            "bot_unavailable",
            409,
        ),
        (RadioApiError::NotFound, "not_found", 404),
        (RadioApiError::QueueFull { max: 3 }, "queue_full", 409),
        (RadioApiError::Unavailable, "unavailable", 503),
    ];
    for (e, code, status) in cases {
        assert_eq!((e.code(), e.http_status()), (code, status));
    }
    assert_eq!(
        engine_error(RadioError::HistoryDisabled),
        RadioApiError::HistoryDisabled
    );
    assert_eq!(engine_error(RadioError::Disabled), RadioApiError::Unavailable);
    assert_eq!(clean_prompt(Some("  ")), None);
    assert_eq!(clean_prompt(Some(" chill ")).as_deref(), Some("chill"));
    assert_eq!(clean_prompt(Some(&"x".repeat(500))).unwrap().len(), 200);
}

#[tokio::test]
async fn history_off_owner_is_rejected() {
    use crate::{
        ai::{test_util::config, AiEngine},
        procs::ProcessRegistry,
        sources::Sources,
        store::{Store, UserPrefsPatch},
    };
    let store = Store::open_in_memory().await.unwrap();
    store
        .update_user_prefs(
            7,
            &UserPrefsPatch {
                locale: None,
                history_enabled: Some(false),
            },
        )
        .await
        .unwrap();
    let dir = std::env::temp_dir().join(format!("lynx-radio-{}", Uuid::new_v4()));
    let cfg = config(dir);
    let sources = Sources::new(&cfg, store.clone(), ProcessRegistry::new()).unwrap();
    let engine = AiEngine::new(&cfg, store, sources);
    let err = engine.check_radio_allowed(7).await.map_err(engine_error);
    assert_eq!(err, Err(RadioApiError::HistoryDisabled));
    assert_eq!(err.unwrap_err().code(), "history_disabled");
    assert_eq!(engine.check_radio_allowed(8).await, Ok(()), "history on by default");
}

/// Collect the commands a driver sends (bounded).
async fn collect(mut rx: mpsc::Receiver<Command>) -> Vec<Command> {
    let mut out = Vec::new();
    while let Ok(Some(cmd)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
        let done = matches!(cmd, Command::RadioDone { .. });
        out.push(cmd);
        if done {
            break;
        }
    }
    out
}

#[tokio::test]
async fn driver_forwards_phases_first_pick_and_result() {
    let (phase_tx, phases) = watch::channel(AiPhase::Queued);
    let (first_tx, first_rx) = oneshot::channel();
    let (go_tx, go_rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let _ = go_rx.await;
        Ok(vec![pick("b"), pick("c")])
    });
    let job = RadioJob::fake(phases, Some(first_rx), handle);
    let id = job.id;
    let (tx, rx) = mpsc::channel(16);
    let driver = tokio::spawn(drive(job, tx));
    let script = tokio::spawn(async move {
        let _ = phase_tx.send(AiPhase::ReadingTaste);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = first_tx.send(pick("a"));
        let _ = phase_tx.send(AiPhase::Thinking);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = phase_tx.send(AiPhase::Ready);
        let _ = go_tx.send(());
    });
    let cmds = collect(rx).await;
    script.await.unwrap();
    driver.await.unwrap();
    let mut phases_seen = Vec::new();
    let mut first = None;
    let mut done = None;
    for c in cmds {
        match c {
            Command::RadioPhase { id: j, phase } => {
                assert_eq!(j, id);
                phases_seen.push(phase);
            }
            Command::RadioPicks { id: j, picks } => {
                assert_eq!(j, id);
                assert!(done.is_none(), "the early pick comes before the result");
                first = Some(picks);
            }
            Command::RadioDone { id: j, result } => {
                assert_eq!(j, id);
                done = Some(*result);
            }
            _ => panic!("unexpected command"),
        }
    }
    assert!(phases_seen.contains(&AiPhase::Thinking), "{phases_seen:?}");
    assert_eq!(phases_seen.last(), Some(&AiPhase::Ready));
    assert_eq!(first.unwrap().len(), 1);
    assert_eq!(done.unwrap().unwrap().len(), 2);
}

#[tokio::test]
async fn driver_reports_failures() {
    let (phase_tx, phases) = watch::channel(AiPhase::Queued);
    let handle = tokio::spawn(async move {
        let _ = phase_tx.send(AiPhase::Failed);
        Err(RadioError::NoCandidates)
    });
    let job = RadioJob::fake(phases, None, handle);
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(drive(job, tx));
    let cmds = collect(rx).await;
    match cmds.last() {
        Some(Command::RadioDone { result, .. }) => {
            assert_eq!(result.as_ref().as_ref().unwrap_err(), &RadioError::NoCandidates)
        }
        _ => panic!("no RadioDone"),
    }
    assert!(!cmds.iter().any(|c| matches!(c, Command::RadioPicks { .. })));
}

/// Live: a real radio job through the player driver (no Discord). Needs LASTFM_API_KEY; OLLAMA_URL
/// and OLLAMA_MODEL are optional.
#[tokio::test]
#[ignore]
async fn radio_driver_live() {
    use crate::{
        ai::{test_util::config, AiEngine, RadioRequest},
        procs::ProcessRegistry,
        sources::{Sources, TrackOrigin},
        store::Store,
    };
    let Some(key) = std::env::var("LASTFM_API_KEY").ok().filter(|k| !k.is_empty()) else {
        eprintln!("LASTFM_API_KEY not set, skipping");
        return;
    };
    let store = Store::open_in_memory().await.unwrap();
    let seeds = [
        ("Daft Punk", "Get Lucky"),
        ("The Weeknd", "Blinding Lights"),
        ("Dua Lipa", "Levitating"),
        ("Tame Impala", "The Less I Know The Better"),
        ("Arctic Monkeys", "Do I Wanna Know?"),
        ("Gorillaz", "Feel Good Inc."),
        ("Phoenix", "1901"),
        ("MGMT", "Electric Feel"),
        ("Two Door Cinema Club", "What You Know"),
        ("Foster The People", "Pumped Up Kicks"),
        ("Glass Animals", "Heat Waves"),
    ];
    for (i, (artist, title)) in seeds.iter().enumerate() {
        let mut t = track(&format!("live{i:02}xxxxxx"));
        t.title = (*title).into();
        t.artists = vec![(*artist).into()];
        t.origin = TrackOrigin::YoutubeMusic;
        store.record_play(7, &t).await.unwrap();
    }
    let dir = std::env::temp_dir().join(format!("lynx-radio-live-{}", Uuid::new_v4()));
    let mut cfg = config(dir);
    cfg.lastfm_api_key = Some(key);
    cfg.ollama_url = std::env::var("OLLAMA_URL").ok().filter(|v| !v.is_empty());
    cfg.ollama_model = std::env::var("OLLAMA_MODEL").ok().filter(|v| !v.is_empty());
    cfg.ollama_timeout_secs = 180;
    let sources = Sources::new(&cfg, store.clone(), ProcessRegistry::new()).unwrap();
    let engine = AiEngine::new(&cfg, store.clone(), sources);
    let tagger = engine.tagger();
    let stop = CancellationToken::new();
    for t in store.pending_tag_tracks(50).await.unwrap() {
        let tags = tagger.tag_one_for_test(&t, &stop).await;
        store.write_track_tags(t.id, &tags).await.unwrap();
    }

    for mix in [RadioMix::Balanced, RadioMix::Discover] {
        let job = engine.start_radio(RadioRequest {
            guild_id: 1,
            channel_id: 2,
            owner_id: 7,
            listener_ids: vec![],
            prompt: Some("upbeat indie for a sunny day".into()),
            recent_keys: vec![],
            batch_size: 10,
            mix,
            blend: true,
        });
        let (tx, rx) = mpsc::channel(64);
        let t0 = std::time::Instant::now();
        tokio::spawn(drive(job, tx));
        let mut rx = rx;
        let mut first_at = None;
        let mut total = 0;
        loop {
            let cmd = tokio::time::timeout(Duration::from_secs(320), rx.recv())
                .await
                .expect("driver stalled")
                .expect("driver ended without RadioDone");
            match cmd {
                Command::RadioPhase { phase, .. } => println!("{:?} {phase:?}", t0.elapsed()),
                Command::RadioPicks { picks, .. } => {
                    first_at = Some(t0.elapsed());
                    total += picks.len();
                    println!(
                        "first pick: {} - {}",
                        picks[0].track.artists.join(", "),
                        picks[0].track.title
                    );
                }
                Command::RadioDone { result, .. } => {
                    let picks = result.expect("radio job failed");
                    total += picks.len();
                    for p in pick_entries(picks, &owner()) {
                        println!("  {} - {} {:?}", p.info.artists.join(", "), p.info.title, p.reason);
                    }
                    break;
                }
                _ => {}
            }
        }
        println!(
            "{mix:?}: {total} picks, first after {first_at:?}, all after {:?}",
            t0.elapsed()
        );
        assert!(total > 1);
    }
}
