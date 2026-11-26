//! Milestone 5 acceptance checks 1 and 3 to 6 against the fake realtime server, each over both
//! transports, and check 8: switching the transport changes nothing else in behaviour, ledger or
//! receipts.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs,
    // Byte counts as f64 in the pacing bound; the wake test reads best as one scenario.
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::chunks_exact_to_as_chunks
)]

#[path = "../../../tests/support/realtime.rs"]
mod realtime;
#[path = "../../../tests/support/voice_host.rs"]
mod voice_host;

use std::sync::Arc;
use std::time::{Duration, Instant};

use cataly_voice::{CallAudio, Control, Passed, Prices, Reply, Settings, Transport};
use realtime::Script;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use voice_host::{FakeHost, frame};

const TEXT: u64 = 222;

fn settings(base: &str, transport: Transport) -> Settings {
    let prices = Prices {
        input: 0.2,
        cached_input: 0.02,
        cache_write: 0.25,
        output: 1.2,
    };
    Settings {
        guild: 1,
        owner: 10,
        voice_channel: 111,
        text_channel: TEXT,
        speech_model: "gpt-live-1".into(),
        backend_model: "gpt-5.6-luna".into(),
        summary_model: "gpt-6-luna".into(),
        voice: "marin".into(),
        backend_prices: prices,
        summary_prices: prices,
        search_price: 0.01,
        reasoning_effort: "low".into(),
        preferences: String::new(),
        zone: "UTC".into(),
        daily_cap: Duration::from_mins(30),
        total_cap: Duration::ZERO,
        spending_cap_usd: 3.0,
        idle_close: Duration::from_secs(120),
        wake_dbfs: -45.0,
        speech_price_per_minute: Some(0.05),
        transport,
        api_base: base.to_string(),
    }
}

struct Harness {
    host: Arc<FakeHost>,
    server: realtime::Server,
    owner: mpsc::Sender<Vec<i16>>,
    play: mpsc::Receiver<Vec<i16>>,
    presence: watch::Sender<bool>,
    control: mpsc::Sender<Control>,
    _stop: watch::Sender<bool>,
    task: JoinHandle<Result<(), String>>,
    n: usize,
    fed: Vec<u8>,
    played: usize,
    first_play: Option<Instant>,
}

async fn harness(
    transport: Transport,
    script: Script,
    host: Arc<FakeHost>,
    tweak: impl FnOnce(&mut Settings),
) -> Harness {
    let server = realtime::start(script).await;
    let mut s = settings(&server.base, transport);
    tweak(&mut s);
    let (owner, owner_rx) = mpsc::channel(1024);
    let (play_tx, play) = mpsc::channel(4096);
    let (presence, presence_rx) = watch::channel(false);
    let (control, control_rx) = mpsc::channel(8);
    let (stop, stop_rx) = watch::channel(false);
    let task = tokio::spawn(cataly_voice::run_with_audio(
        Arc::new(s),
        "sk-test".into(),
        Arc::clone(&host),
        CallAudio {
            owner: owner_rx,
            play: play_tx,
        },
        presence_rx,
        control_rx,
        stop_rx,
    ));
    Harness {
        host,
        server,
        owner,
        play,
        presence,
        control,
        _stop: stop,
        task,
        n: 0,
        fed: Vec::new(),
        played: 0,
        first_play: None,
    }
}

impl Harness {
    /// Feeds `ms` of the recording, one 20 ms frame every `step`.
    async fn feed(&mut self, ms: u32, loud: bool, step: Duration) {
        let mut tick = tokio::time::interval(step);
        for _ in 0..ms / 20 {
            tick.tick().await;
            let f = frame(loud, self.n);
            self.n += 1;
            self.fed.extend(f.iter().flat_map(|s| s.to_le_bytes()));
            self.owner.send(f).await.unwrap();
            self.drain_play();
        }
    }

    async fn speak(&mut self, ms: u32, loud: bool) {
        self.feed(ms, loud, Duration::from_millis(20)).await;
    }

    fn drain_play(&mut self) {
        // Silent frames (the provider's idle output) are not counted as played.
        while let Ok(frame) = self.play.try_recv() {
            if frame.iter().all(|&s| s == 0) {
                continue;
            }
            self.played += 1;
            self.first_play.get_or_insert_with(Instant::now);
        }
    }

    /// Feeds quiet audio until `cond` holds or `limit` passes.
    async fn until(&mut self, limit: Duration, mut cond: impl FnMut(&Self) -> bool) -> bool {
        let end = Instant::now() + limit;
        while Instant::now() < end {
            if cond(self) {
                return true;
            }
            self.speak(100, false).await;
        }
        cond(self)
    }

    fn sessions(&self) -> usize {
        self.server.log.lock().unwrap().sessions.len()
    }

    fn events(&self, kind: &str) -> Vec<serde_json::Value> {
        self.server.log.lock().unwrap().events_of(kind)
    }
}

macro_rules! both {
    ($name:ident, $body:ident) => {
        mod $name {
            use super::*;
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn websocket() {
                $body(Transport::WebSocket).await;
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn webrtc() {
                $body(Transport::WebRtc).await;
            }
        }
    };
}

/// Check 3: wake on speech sends the 2.5 s lead-in first, without a greeting; input is paced at
/// 1.15x real time.
async fn wake_lead_in_and_pace(t: Transport) {
    let script = Script {
        greet_ms: 200,
        ..Script::default()
    };
    let mut h = harness(t, script, FakeHost::new(), |s| {
        s.idle_close = Duration::from_secs(2);
    })
    .await;
    h.presence.send(true).unwrap();
    assert!(
        h.until(Duration::from_secs(10), |h| h.sessions() == 1)
            .await
    );
    // Idle close of the greeting session.
    assert!(
        h.until(Duration::from_secs(10), |h| !h
            .events("session.close")
            .is_empty())
            .await
    );
    h.speak(1000, false).await;
    let before = h.fed.len();
    h.speak(3000, false).await;
    // Speech wakes a new session; then 2 s of audio arrives at twice real time.
    h.speak(400, true).await;
    assert!(
        h.until(Duration::from_secs(10), |h| h.sessions() == 2)
            .await
    );
    h.feed(2000, true, Duration::from_millis(10)).await;
    h.speak(1500, false).await;
    let log = h.server.log.lock().unwrap();
    assert!(log.sessions[0].get("input").is_some(), "presence greets");
    assert!(
        log.sessions[1].get("input").is_none(),
        "wake does not greet"
    );
    let arrivals: Vec<(Instant, usize)> = log
        .input
        .iter()
        .filter(|(i, _, _)| *i == 1)
        .map(|(_, t, n)| (*t, *n))
        .collect();
    let first = arrivals[0].0;
    let burst: usize = arrivals
        .iter()
        .filter(|(at, _)| at.duration_since(first) < Duration::from_millis(300))
        .map(|(_, n)| n)
        .sum();
    // The lead-in ends with the speech that woke the session: quiet first, loud at its end.
    let rms = |b: &[u8]| {
        let s: Vec<f64> = b
            .chunks_exact(2)
            .map(|x| f64::from(i16::from_le_bytes([x[0], x[1]])))
            .collect();
        (s.iter().map(|v| v * v).sum::<f64>() / s.len() as f64).sqrt()
    };
    let received = &log.input_bytes[1];
    assert!(
        rms(&received[..100_000]) < 300.0,
        "lead-in starts with the quiet before the speech"
    );
    assert!(
        rms(&received[112_000..120_000]) > 1_000.0,
        "lead-in ends with the waking speech"
    );
    if t == Transport::WebSocket {
        assert!(burst >= 120_000, "lead-in goes out at once: {burst}");
        let got = &received[..120_000];
        let fed = &h.fed[before..];
        let at = fed
            .windows(120_000)
            .position(|w| w == got)
            .expect("the lead-in is 2.5 s of the owner's audio, in order");
        assert_eq!(
            &fed[at + 120_000..at + 121_920],
            &received[120_000..121_920],
            "then the live audio"
        );
    }
    let mut total = 0usize;
    for (at, n) in &arrivals {
        total += n;
        let elapsed = at.duration_since(first).as_secs_f64();
        // Slack: one frame for WebSocket; 200 ms for WebRTC, whose packets reach the fake through
        // str0m and SCTP/SRTP queues, so arrival times trail the send times unevenly.
        let slack = if t == Transport::WebSocket {
            4_000.0
        } else {
            9_600.0
        };
        let limit = 144_000.0 + 1.15 * 48_000.0 * elapsed + slack;
        assert!(
            (total as f64) <= limit,
            "paced: {total} bytes after {elapsed:.3}s; first arrivals {:?}",
            arrivals
                .iter()
                .take(12)
                .map(|(a, n)| (a.duration_since(first).as_millis(), *n))
                .collect::<Vec<_>>()
        );
    }
    drop(log);
    h.task.abort();
}
both!(check3_wake_and_pace, wake_lead_in_and_pace);

/// Check 3: barge-in clears playback. Also measures speech end to first audio out.
async fn barge_in(t: Transport) {
    let script = Script {
        reply_ms: 3000,
        reply_once: true,
        ..Script::default()
    };
    let mut h = harness(t, script, FakeHost::new(), |_| {}).await;
    h.presence.send(true).unwrap();
    assert!(
        h.until(Duration::from_secs(10), |h| h.sessions() == 1)
            .await
    );
    h.speak(600, true).await;
    let speech_end = Instant::now();
    assert!(h.until(Duration::from_secs(10), |h| h.played > 0).await);
    let latency = h.first_play.unwrap().duration_since(speech_end);
    eprintln!(
        "MEASURE {t:?} speech end to first audio out: {} ms",
        latency.as_millis()
    );
    // Owner talks over the reply for 500 ms while over 400 ms is queued.
    h.speak(500, true).await;
    h.speak(4000, false).await;
    assert!(
        h.played < 150,
        "barge-in cleared the 3 s reply; played {} frames",
        h.played
    );
    h.task.abort();
}
both!(check3_barge_in, barge_in);

/// Check 4: pass-to-lead reports exactly one of three outcomes and never resends.
async fn pass_outcomes(t: Transport) -> Vec<String> {
    let args = |r: &str| format!(r#"{{"request":"{r}","high_risk":false}}"#);
    let script = Script {
        reply_ms: 300,
        // The first call is answered before the later ones arrive and the response completes.
        call_gap_ms: 300,
        tools: vec![
            ("pass_to_lead".into(), args("check the build")),
            ("pass_to_lead".into(), args("rerun the tests")),
            ("pass_to_lead".into(), args("deploy `prod` $now")),
            (
                "read_status".into(),
                r#"{"parts":["lanes"],"early_update":false}"#.into(),
            ),
        ],
        ..Script::default()
    };
    let host = FakeHost::new();
    *host.outcomes.lock().unwrap() = vec![
        Passed::Received,
        Passed::Unproven,
        Passed::Refused("the lead has a pending approval".into()),
    ];
    let mut h = harness(t, script, Arc::clone(&host), |_| {}).await;
    h.presence.send(true).unwrap();
    assert!(
        h.until(Duration::from_secs(10), |h| h.sessions() == 1)
            .await
    );
    h.speak(600, true).await;
    assert!(
        h.until(Duration::from_secs(15), |h| !h
            .events("response.create")
            .is_empty())
            .await
    );
    // Both backend responses are counted before comparing the ledger (recorded each second).
    assert!(
        h.until(Duration::from_secs(5), |h| {
            let spend: f64 = h
                .host
                .seen
                .lock()
                .unwrap()
                .records
                .iter()
                .map(|r| r.2)
                .sum();
            spend > 0.00044
        })
        .await
    );
    h.speak(500, false).await;
    let outputs: Vec<String> = h
        .events("response.item.create")
        .iter()
        .map(|v| v["item"]["output"].as_str().unwrap().to_string())
        .collect();
    let passes = host.seen.lock().unwrap().passes.clone();
    assert_eq!(passes.len(), 3, "each request sent exactly once");
    assert!(
        passes[2].1.contains("deploy prod now"),
        "request cleaned: {}",
        passes[2].1
    );
    assert!(passes.iter().all(|(id, text)| text.contains(id.as_str())));
    let pass_out: Vec<&String> = outputs.iter().filter(|o| o.contains("delivered")).collect();
    assert_eq!(pass_out.len(), 3);
    assert!(pass_out[0].contains(r#""delivered":true"#));
    assert!(
        pass_out[1].contains("receipt unproven") && pass_out[1].contains(r#""delivered":false"#)
    );
    assert!(pass_out[2].contains("not delivered: the lead has a pending approval"));
    assert_eq!(
        h.events("response.create").len(),
        1,
        "continued once, after every call"
    );
    h.task.abort();
    let mut summary: Vec<String> = outputs
        .iter()
        .map(|o| {
            let v: serde_json::Value = serde_json::from_str(o).unwrap();
            v["result"]
                .as_str()
                .map_or_else(|| "status".to_string(), str::to_string)
        })
        .collect();
    summary.sort();
    let seen = host.seen.lock().unwrap();
    summary.push(format!(
        "sessions={}",
        seen.records.iter().map(|r| r.1).sum::<u32>()
    ));
    summary.push(format!("passes={}", seen.passes.len()));
    let spend: f64 = seen.records.iter().map(|r| r.2).sum();
    summary.push(format!("spend={spend:.6}"));
    summary
}
async fn pass_outcomes_unit(t: Transport) {
    let _ = pass_outcomes(t).await;
}
both!(check4_pass_to_lead, pass_outcomes_unit);

/// Check 8: the same scenario over both transports gives the same receipts, ledger and outcomes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check8_transports_agree() {
    let ws = pass_outcomes(Transport::WebSocket).await;
    let rtc = pass_outcomes(Transport::WebRtc).await;
    assert_eq!(ws, rtc);
}

/// The fake answers a client event with an undocumented or missing field the way the real API
/// does, so a stray field fails here first.
#[test]
fn strict_fake_rejects_undocumented_fields() {
    use serde_json::json;
    let err = realtime::check(
        &json!({"type": "response.create", "event_id": "e1", "delegation_id": "d1"}),
        true,
    )
    .unwrap_err();
    assert_eq!(err["code"], "unknown_parameter");
    assert_eq!(err["message"], "Unknown parameter: 'delegation_id'.");
    assert_eq!(err["param"], "delegation_id");
    assert!(realtime::check(&json!({"type": "response.create", "event_id": "e1"}), true).is_ok());
    let append = |extra: serde_json::Value| {
        let mut v = json!({"type": "session.commentary.append", "event_id": "e2", "content": "x"});
        if let (Some(v), Some(extra)) = (v.as_object_mut(), extra.as_object()) {
            v.extend(extra.clone());
        }
        realtime::check(&v, true)
    };
    assert!(append(json!({"delegation_id": null})).is_ok());
    assert_eq!(
        append(json!({})).unwrap_err()["code"],
        "missing_required_parameter"
    );
    assert_eq!(
        append(json!({"delegation_id": "d1"})).unwrap_err()["param"],
        "delegation_id"
    );
    assert_eq!(
        realtime::check_session(&json!({"model": "gpt-live-1", "voice": "marin"})).unwrap_err()["param"],
        "session.voice"
    );
}

/// A delegation whose tool output goes back is continued, and a lead reply that arrives meanwhile
/// is spoken afterwards, while the provider streams silence between replies as the real API does:
/// every client event on that path is one the API documents, so the strict fake rejects none and
/// voice is not held (0.2.2 sent `delegation_id` on `response.create`), and the streamed silence
/// neither counts as output nor keeps the reply from a pause.
async fn delegation_then_reply(t: Transport) {
    let script = Script {
        reply_ms: 300,
        call_gap_ms: 300,
        tools: vec![
            (
                "read_status".into(),
                r#"{"parts":["lanes"],"early_update":true}"#.into(),
            ),
            (
                "pass_to_lead".into(),
                r#"{"request":"check the build","high_risk":false}"#.into(),
            ),
        ],
        ack_commentary: true,
        cover_commentary: true,
        commentary_ms: 600,
        idle_audio: true,
        ..Script::default()
    };
    let host = FakeHost::new();
    let mut h = harness(t, script, Arc::clone(&host), |_| {}).await;
    h.presence.send(true).unwrap();
    assert!(
        h.until(Duration::from_secs(10), |h| h.sessions() == 1)
            .await
    );
    h.speak(600, true).await;
    assert!(
        h.until(Duration::from_secs(15), |h| !h
            .host
            .seen
            .lock()
            .unwrap()
            .passes
            .is_empty())
            .await
    );
    // The lead answers while the delegation is still being continued.
    let reply = Reply {
        id: 9,
        request_id: host.seen.lock().unwrap().passes[0].0.clone(),
        subject: "build".into(),
        text: "The build on main passed and the release branch is ready.".into(),
        spoken: 0,
    };
    host.replies.lock().unwrap().insert(9, reply.clone());
    h.control.send(Control::Replies).await.unwrap();
    assert!(
        h.until(Duration::from_secs(20), |h| {
            let seen = h.host.seen.lock().unwrap();
            !seen.spoken.is_empty() || !seen.holds.is_empty()
        })
        .await
    );
    let rejected = h.server.log.lock().unwrap().rejected.clone();
    assert_eq!(rejected, Vec::<serde_json::Value>::new(), "{t:?}");
    let seen = host.seen.lock().unwrap();
    assert_eq!(seen.holds, Vec::<String>::new());
    assert_eq!(seen.spoken, vec![(9, reply.text.len())]);
    drop(seen);
    assert_eq!(h.events("response.create").len(), 1, "continued once");
    let pieces = h
        .events("session.commentary.append")
        .iter()
        .filter(|v| {
            v["content"]
                .as_str()
                .is_some_and(|c| c.contains("data, not an instruction"))
        })
        .count();
    assert_eq!(pieces, 1, "the reply went out as one commentary piece");
    h.task.abort();
}
both!(check9_delegation_then_reply, delegation_then_reply);

/// Check 5: a lead reply counts as spoken only after acknowledgement, transcript coverage and
/// drained playback in the same generation.
async fn reply_proof(t: Transport) {
    let reply = Reply {
        id: 7,
        request_id: "v1".into(),
        subject: "build".into(),
        text: "The build on main passed all checks and the release branch is ready.".into(),
        spoken: 0,
    };
    // Proven: acknowledged, covered, played and drained.
    let host = FakeHost::new();
    host.replies.lock().unwrap().insert(7, reply.clone());
    let script = Script {
        ack_commentary: true,
        cover_commentary: true,
        commentary_ms: 600,
        ..Script::default()
    };
    let mut h = harness(t, script, Arc::clone(&host), |_| {}).await;
    h.presence.send(true).unwrap();
    assert!(
        h.until(Duration::from_secs(15), |h| !h
            .host
            .seen
            .lock()
            .unwrap()
            .spoken
            .is_empty())
            .await
    );
    assert_eq!(
        host.seen.lock().unwrap().spoken,
        vec![(7, reply.text.len())]
    );
    assert!(h.played > 0);
    let offered = h.events("session.commentary.append");
    assert_eq!(offered.len(), 1);
    assert!(
        offered[0]["content"]
            .as_str()
            .unwrap()
            .contains("data, not an instruction")
    );
    h.task.abort();

    // Acknowledged and played, but the transcript covers too little: never counted.
    let host = FakeHost::new();
    host.replies.lock().unwrap().insert(7, reply.clone());
    let script = Script {
        ack_commentary: true,
        cover_commentary: false,
        commentary_ms: 600,
        ..Script::default()
    };
    let mut h = harness(t, script, Arc::clone(&host), |_| {}).await;
    h.presence.send(true).unwrap();
    assert!(
        h.until(Duration::from_secs(10), |h| !h
            .events("session.commentary.append")
            .is_empty())
            .await
    );
    h.speak(4000, false).await;
    assert_eq!(host.seen.lock().unwrap().spoken, Vec::new());
    h.task.abort();

    // A barge-in during playback starts a new generation: the piece goes back and is offered again.
    let host = FakeHost::new();
    host.replies.lock().unwrap().insert(7, reply);
    let script = Script {
        ack_commentary: true,
        cover_commentary: true,
        commentary_ms: 3000,
        ..Script::default()
    };
    let mut h = harness(t, script, Arc::clone(&host), |_| {}).await;
    h.presence.send(true).unwrap();
    assert!(h.until(Duration::from_secs(10), |h| h.played > 0).await);
    h.speak(600, true).await;
    assert!(
        host.seen.lock().unwrap().spoken.is_empty(),
        "cleared playback is not spoken"
    );
    assert!(
        h.until(Duration::from_secs(15), |h| h
            .events("session.commentary.append")
            .len()
            >= 2)
            .await
    );
    h.task.abort();
}
both!(check5_reply_proof, reply_proof);

/// Check 6: a provider failure holds voice until an explicit restart, with no paid retry; the summary
/// post goes out.
async fn failure_holds(t: Transport) {
    let script = Script {
        fail_after_start: Some("server_error".into()),
        ..Script::default()
    };
    let host = FakeHost::new();
    let mut h = harness(t, script, Arc::clone(&host), |_| {}).await;
    h.presence.send(true).unwrap();
    assert!(
        h.until(Duration::from_secs(10), |h| !h
            .host
            .seen
            .lock()
            .unwrap()
            .holds
            .is_empty())
            .await
    );
    assert!(
        h.until(Duration::from_secs(10), |h| h
            .host
            .seen
            .lock()
            .unwrap()
            .posts
            .iter()
            .any(|(_, p)| p.contains("Voice session summary")))
            .await
    );
    // Rejoining and speaking does not start another (paid) session.
    h.presence.send(false).unwrap();
    h.speak(200, false).await;
    h.presence.send(true).unwrap();
    h.speak(600, true).await;
    h.speak(1000, false).await;
    assert_eq!(h.sessions(), 1, "no automatic retry");
    {
        let seen = host.seen.lock().unwrap();
        assert!(seen.holds[0].contains("server_error"));
        assert!(
            seen.posts
                .iter()
                .any(|(c, p)| *c == TEXT && p.contains("Voice is held"))
        );
        assert!(
            seen.posts
                .iter()
                .any(|(_, p)| p.contains("no model summary: the session failed")
                    && p.contains("Owner said: hello"))
        );
    }
    assert!(
        h.server.log.lock().unwrap().summaries.is_empty(),
        "no model call after a failure"
    );
    // An explicit restart lifts the hold.
    *host.held.lock().unwrap() = None;
    h.control.send(Control::Restart).await.unwrap();
    assert!(
        h.until(Duration::from_secs(10), |h| h.sessions() == 2)
            .await
    );
    h.task.abort();
}
both!(check6_failure_holds, failure_holds);

/// The real API initializes a session before it starts it, which takes seconds over WebRTC (the
/// session POST is answered only then, and ICE, DTLS and SCTP follow). The owner's audio arriving
/// meanwhile neither fails the session nor piles up: once live, the newest 3 s go out, paced.
async fn slow_start(t: Transport) {
    let script = Script {
        start_delay_ms: 4000,
        greet_ms: 200,
        ..Script::default()
    };
    let host = FakeHost::new();
    let mut h = harness(t, script, Arc::clone(&host), |_| {}).await;
    h.presence.send(true).unwrap();
    let live = h
        .until(Duration::from_secs(15), |h| {
            h.played > 0 && !h.server.log.lock().unwrap().input.is_empty()
        })
        .await;
    h.speak(1000, false).await;
    assert!(
        live && host.seen.lock().unwrap().holds.is_empty(),
        "the session went live without a failure: {:?}",
        host.seen.lock().unwrap().holds
    );
    let log = h.server.log.lock().unwrap();
    let arrivals: Vec<(Instant, usize)> = log.input.iter().map(|(_, t, n)| (*t, *n)).collect();
    let first = arrivals[0].0;
    let mut total = 0usize;
    for (at, n) in &arrivals {
        total += n;
        let elapsed = at.duration_since(first).as_secs_f64();
        let limit = 144_000.0 + 1.15 * 48_000.0 * elapsed + 9_600.0;
        assert!(
            (total as f64) <= limit,
            "at most the newest 3 s at once, then paced: {total} bytes after {elapsed:.3}s"
        );
    }
    drop(log);
    h.task.abort();
}
both!(check3_slow_start, slow_start);

/// Check 1: never starts while a cap is reached (one notice); a corrupt ledger stops voice and is
/// never reset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check1_caps_and_corrupt_ledger() {
    let host = FakeHost::new();
    host.day.lock().unwrap().spend_usd = 3.0;
    let mut h = harness(
        Transport::WebSocket,
        Script::default(),
        Arc::clone(&host),
        |_| {},
    )
    .await;
    h.presence.send(true).unwrap();
    h.speak(600, true).await;
    h.speak(400, false).await;
    assert_eq!(h.sessions(), 0);
    assert!(
        host.seen
            .lock()
            .unwrap()
            .posts
            .iter()
            .any(|(_, p)| p.contains("spending cap"))
    );
    h.task.abort();

    let mut host = FakeHost::new();
    Arc::get_mut(&mut host).unwrap().corrupt = true;
    let mut h = harness(
        Transport::WebSocket,
        Script::default(),
        Arc::clone(&host),
        |_| {},
    )
    .await;
    h.presence.send(true).unwrap();
    h.speak(600, true).await;
    assert_eq!(h.sessions(), 0);
    let seen = host.seen.lock().unwrap();
    assert_eq!(seen.holds, vec!["ledger corrupt".to_string()]);
    assert!(seen.records.is_empty(), "a corrupt ledger is never written");
    drop(seen);
    h.task.abort();
}

/// A cap reached during a call: the limit announcement, then close 8 s later; summary skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cap_mid_call_winds_down() {
    let host = FakeHost::new();
    host.day.lock().unwrap().speech_seconds = 1798.0;
    let script = Script {
        reply_ms: 300,
        ..Script::default()
    };
    let mut h = harness(Transport::WebSocket, script, Arc::clone(&host), |_| {}).await;
    h.presence.send(true).unwrap();
    assert!(
        h.until(Duration::from_secs(10), |h| !h
            .events("session.instructions.append")
            .is_empty())
            .await
    );
    let limit = h.events("session.instructions.append");
    assert!(
        limit[0]["content"]
            .as_str()
            .unwrap()
            .contains("allowance is used up")
    );
    let announced = Instant::now();
    assert!(
        h.until(Duration::from_secs(12), |h| !h
            .events("session.close")
            .is_empty())
            .await
    );
    assert!(announced.elapsed() >= Duration::from_secs(7));
    h.task.abort();
}
