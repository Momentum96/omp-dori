#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Mutex as StdMutex;

use super::*;
use crate::host::testing::spec;

type Seen = Arc<StdMutex<Vec<Value>>>;

/// A bridge stand-in: records each command and answers it with `answer`.
fn fake_bridge(
    port: &Arc<Port>,
    answer: impl Fn(&Value) -> Value + Send + 'static,
) -> (Seen, mpsc::Sender<Response>) {
    let (tx, mut rx) = mpsc::channel::<Response>(64);
    let seen: Seen = Arc::default();
    let (port, log) = (Arc::clone(port), Arc::clone(&seen));
    tokio::spawn(async move {
        while let Some(response) = rx.recv().await {
            let line: Value = serde_json::from_str(response.to_line().trim()).unwrap();
            let cmd = line["item"]["cmd"].clone();
            if cmd.is_null() {
                continue;
            }
            log.lock().unwrap().push(cmd.clone());
            let mut reply = answer(&cmd);
            reply["n"] = cmd["n"].clone();
            port.reply(&reply);
        }
    });
    (seen, tx)
}

fn hello(session: &str, pid: u64) -> Value {
    json!({ "session": session, "file": format!("/tmp/{session}.jsonl"), "pid": pid, "idle": true,
        "model": "fake/primary", "goal": { "status": "active" } })
}

fn port() -> (Arc<Port>, oneshot::Receiver<()>) {
    Port::new(
        &spec("t1", None),
        std::env::temp_dir(),
        Latency::default(),
        Duration::from_millis(300),
    )
}

async fn bound(answer: impl Fn(&Value) -> Value + Send + 'static) -> (Session, Seen) {
    let (port, attached) = port();
    let (seen, tx) = fake_bridge(&port, answer);
    port.attach(&hello("s1", 42), "k1", tx).unwrap();
    attached.await.unwrap();
    let started = port.bind(&spec("t1", None)).unwrap();
    assert_eq!(
        (started.session_id.as_str(), started.pid, started.held),
        ("s1", Some(42), None)
    );
    (Session { port }, seen)
}

fn delivery(text: &str, steer: bool) -> Delivery {
    Delivery {
        expected_session: "s1".into(),
        text: text.into(),
        timing: Timing::Any,
        steer,
        fresh: true,
    }
}

#[tokio::test]
async fn missing_tern_is_refused_with_an_install_hint() {
    let tern = Tern {
        program: None,
        block_session: None,
        omp: "omp".into(),
        state: std::env::temp_dir(),
        socket: "/tmp/x.sock".into(),
        spill: std::env::temp_dir(),
        extra_flags: Vec::new(),
        agent_start: Latency::default(),
        links: Arc::default(),
    };
    let Err(StartError::Refused(reason)) = tern.start(spec("t0", None)).await else {
        panic!("a start without Tern must be refused");
    };
    assert!(reason.contains("install Tern"), "{reason}");
}

#[test]
fn the_block_runs_omp_with_the_token_and_without_rpc_flags() {
    let tern = Tern {
        program: Some("/bin/tern".into()),
        block_session: Some("work".into()),
        omp: "/bin/omp".into(),
        state: "/state".into(),
        socket: "/run/hub.sock".into(),
        spill: "/state/files".into(),
        extra_flags: Vec::new(),
        agent_start: Latency::default(),
        links: Arc::default(),
    };
    let s = spec("t2", None);
    let argv = tern.argv(Path::new("/bin/tern"), &s, "tok");
    let head: Vec<&str> = argv.iter().take(5).map(String::as_str).collect();
    assert_eq!(head, ["/bin/tern", "new", "tab", "work", "--json"]);
    let at = argv.iter().position(|a| a == "--").unwrap();
    assert_eq!(argv[at + 1], "/usr/bin/env");
    assert_eq!(argv[at + 2], "CATALY_LAUNCH=tok");
    assert_eq!(argv[at + 3], "CATALY_SOCKET=/run/hub.sock");
    assert_eq!(argv[at + 4], "PI_NOTIFICATIONS=off");
    assert!(argv[at + 5..].iter().any(|a| a == "/bin/omp"));
    assert!(!argv.iter().any(|a| a == "rpc-ui" || a == "--approval-mode"));
    assert!(argv.iter().any(|a| a == "--model=fake/primary"));
    assert_eq!(block_id(br#"{"id":"b7"}"#).as_deref(), Some("b7"));
}

#[tokio::test]
async fn deliveries_go_through_the_bridge_and_not_current_keeps_them_pending() {
    let (session, seen) = bound(|cmd| match cmd["id"].as_str() {
        Some("busy0001") => json!({ "refused": "not current" }),
        _ => json!({ "sent": true }),
    })
    .await;
    assert_eq!(session.snapshot().status, Status::Idle);
    let out = session
        .deliver(delivery("[cataly:ab12cd34] hello", false))
        .await;
    assert!(matches!(
        out,
        DeliverOutcome::Delivered {
            turn_started: true,
            ..
        }
    ));
    let first = seen.lock().unwrap()[0].clone();
    assert_eq!(
        (
            first["op"].as_str(),
            first["id"].as_str(),
            first["mode"].as_str()
        ),
        (Some("deliver"), Some("ab12cd34"), Some("wake"))
    );
    assert_eq!(first["session"], "s1");
    // OMP stores the delivery before its turn starts: working from the reply on, not idle.
    assert_eq!(session.snapshot().status, Status::Working);
    session.port.event(&json!({ "kind": "agent_start" }));
    session.port.event(&json!({ "kind": "agent_end" }));
    assert_eq!(session.snapshot().status, Status::Idle);
    // A user's switch in progress: refused "not current" and kept pending (check 2).
    let out = session
        .deliver(delivery("[cataly:busy0001] later", false))
        .await;
    assert_eq!(out, DeliverOutcome::NotReady(Status::Idle));
    // Steering needs a running turn; another session is never written.
    assert!(matches!(
        session.deliver(delivery("x", true)).await,
        DeliverOutcome::Refused(_)
    ));
    let mut other = delivery("x", false);
    other.expected_session = "s9".into();
    assert!(matches!(
        session.deliver(other).await,
        DeliverOutcome::NotReady(_)
    ));
    // A turn starts and the session is working: a steer goes as a steer.
    session.port.event(&json!({ "kind": "agent_start" }));
    assert_eq!(session.snapshot().status, Status::Working);
    assert!(matches!(
        session
            .deliver(delivery("[cataly:5e5e5e5e] steer", true))
            .await,
        DeliverOutcome::Delivered {
            turn_started: false,
            ..
        }
    ));
    assert_eq!(seen.lock().unwrap().last().unwrap()["mode"], "steer");
    session
        .port
        .event(&json!({ "kind": "agent_end", "will_continue": false }));
    assert_eq!(session.snapshot().status, Status::Idle);
    // Over 64 KiB goes to a file the agent is pointed at.
    let big = format!("[cataly:b16b16b1]\n{}", "y".repeat(70 * 1024));
    let _ = session.deliver(delivery(&big, false)).await;
    let text = seen.lock().unwrap().last().unwrap()["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.starts_with("[cataly:b16b16b1]") && text.contains("delivery-b16b16b1.txt"));
    let _ = std::fs::remove_file(std::env::temp_dir().join("delivery-b16b16b1.txt"));
}

#[tokio::test]
async fn interrupt_model_goal_and_shutdown_are_bridge_commands() {
    // A create is refused "busy" once (OMP restoring its saved modes), then enters goal mode.
    let refused = AtomicBool::new(false);
    let (session, seen) = bound(move |cmd| match cmd["op"].as_str() {
        Some("interrupt") => json!({ "idle": true }),
        Some("model") => json!({ "model": "fake/second" }),
        Some("goal") if cmd["action"] == "enter" => {
            if refused.swap(true, Ordering::SeqCst) {
                json!({ "goal": { "status": "active", "objective": cmd["objective"] } })
            } else {
                json!({ "refused": "busy: The session's saved modes are still being restored" })
            }
        }
        Some("goal") => json!({ "goal": { "status": "paused" } }),
        Some("state") => json!({ "idle": true, "session": "s1", "file": "/tmp/s1.jsonl",
            "goal": { "status": "paused" } }),
        _ => json!({}),
    })
    .await;
    assert_eq!(session.interrupt().await.unwrap(), "idle");
    assert_eq!(
        session.set_model("fake/second:high").await.unwrap(),
        "fake/second"
    );
    assert_eq!(session.snapshot().model.as_deref(), Some("fake/second"));
    let goal = session.goal("pause", None).await.unwrap();
    assert_eq!(goal.status, GoalStatus::Paused);
    assert_eq!(
        session.goal("get", None).await.unwrap().status,
        GoalStatus::Paused
    );
    let goal = session.goal("create", Some("x")).await.unwrap();
    assert_eq!(
        (goal.status, goal.objective.as_deref()),
        (GoalStatus::Active, Some("x"))
    );
    assert!(session.goal("drop", None).await.is_err());
    let ops: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|c| c["op"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ops, ["interrupt", "model", "goal", "state", "goal", "goal"]);
    // Shutdown: asked, then OMP reports it is going.
    let port = Arc::clone(&session.port);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        port.event(&json!({ "kind": "session_shutdown" }));
    });
    let record = session.close(false).await.unwrap();
    assert_eq!(record.how, "clean");
    assert!(!session.alive());
}

#[tokio::test]
async fn approvals_block_and_are_never_answered_but_mirrored_forms_are() {
    let (session, seen) = bound(|_| json!({})).await;
    session
        .port
        .event(&json!({ "kind": "tool_approval_requested", "call": "c1", "tool": "bash" }));
    let snap = session.snapshot();
    assert_eq!(snap.status, Status::Blocked);
    let err = session
        .answer("s1", "c1", &json!({ "value": "Approve once" }))
        .await
        .unwrap_err();
    assert!(err.contains("person at the terminal"), "{err}");
    session
        .port
        .event(&json!({ "kind": "tool_approval_resolved", "call": "c1" }));
    assert_eq!(session.snapshot().status, Status::Idle);
    // A form the bridge offers becomes an ask dialog the relays may post (check 4).
    let input = json!({ "questions": [{ "id": "q", "question": "Color?",
        "options": [{ "label": "Red" }, { "label": "Blue" }] }] });
    let offer = session.port.form(&json!({ "call": "f1", "input": input }));
    assert_eq!(offer, json!({ "mirrored": true }));
    let snap = session.snapshot();
    assert_eq!(snap.dialogs[0].kind, DialogKind::Ask);
    assert_eq!(
        session
            .port
            .form(&json!({ "call": "f2", "input": { "questions": [] } })),
        json!({ "mirrored": false })
    );
    let answer = json!({ "answers": [{ "id": "q", "selectedOptions": ["Red"] }] });
    session.answer("s1", "f1", &answer).await.unwrap();
    let last = seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        (last["op"].as_str(), last["call"].as_str()),
        (Some("form"), Some("f1"))
    );
    assert_eq!(session.snapshot().dialogs, Vec::<Dialog>::new());
    // The local answer won another form: the bridge reports it closed.
    session.port.form(&json!({ "call": "f3", "input": input }));
    session
        .port
        .event(&json!({ "kind": "form_closed", "call": "f3", "by": "the terminal" }));
    assert_eq!(session.snapshot().dialogs, Vec::<Dialog>::new());
}

#[tokio::test]
async fn a_silent_link_is_lost_after_45_s_and_the_same_process_restores_it() {
    let (session, _) = bound(|_| json!({})).await;
    let mut events = session.subscribe();
    session.port.detach("k1");
    assert_eq!(session.snapshot().status, Status::Unknown);
    assert!(matches!(
        session.deliver(delivery("x", false)).await,
        DeliverOutcome::NotReady(Status::Unknown)
    ));
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (_, lost) = events.recv().await.unwrap();
    assert_eq!(lost, SessionEvent::LinkLost);
    assert!(session.snapshot().reason.clone().unwrap().contains("lost"));
    // Another process with the same token's session is refused.
    let (tx, _rx) = mpsc::channel(4);
    assert!(session.port.attach(&hello("s1", 99), "k2", tx).is_err());
    // The same process with the same session restores it (check 5).
    let (_, tx) = fake_bridge(&session.port, |_| json!({}));
    session.port.attach(&hello("s1", 42), "k3", tx).unwrap();
    let (_, restored) = events.recv().await.unwrap();
    assert_eq!(restored, SessionEvent::LinkRestored);
    assert_eq!(session.snapshot().status, Status::Idle);
    assert!(session.snapshot().reason.is_none());
    // The same process back with another session: a new identity, the old one is ended.
    session.port.detach("k3");
    let (tx, _rx) = mpsc::channel(4);
    assert!(session.port.attach(&hello("s2", 42), "k4", tx).is_err());
    assert!(!session.alive());
}

#[tokio::test]
async fn a_user_switch_moves_the_current_session_and_deliveries_follow_it() {
    let (session, _) = bound(|_| json!({ "sent": true })).await;
    session
        .port
        .event(&json!({ "kind": "session_switch", "session": "s2", "file": "/tmp/s2.jsonl" }));
    let snap = session.snapshot();
    assert_eq!(snap.session_id.as_deref(), Some("s2"));
    // A delivery recorded for s1 is not written into s2 (PATCH-01 rule 2).
    assert!(matches!(
        session.deliver(delivery("[cataly:aa] x", false)).await,
        DeliverOutcome::NotReady(_)
    ));
}
