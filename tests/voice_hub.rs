//! Milestone 5 acceptance checks through the real hub: the fake Discord (REST and Gateway with
//! voice-state and voice-server updates), the fake Discord voice server (voice WebSocket, UDP,
//! transport encryption), the fake realtime server on both transports and the fake OMP lead.
//!
//! Checks 1 (settings, ledger), 2 (owner-only receive, resubscribe, plaintext window), 3 (wake,
//! pacing, barge-in), 4 and 8 (pass-to-lead outcomes, transports agree), 5 (reply proof) and 6
//! (holds); check 7 is in `tests/transcribe.rs`.

#![cfg(feature = "voice")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    clippy::print_stderr,
    clippy::chunks_exact_to_as_chunks,
    clippy::assert_is_empty
)]

#[path = "support/discord_voice.rs"]
mod discord_voice;
#[path = "support/realtime.rs"]
mod realtime;
mod support;
#[path = "support/voice_host.rs"]
mod voice_host;

use std::time::{Duration, Instant};

use discord_voice::{VoiceServer, opus};
use realtime::Script;
use serde_json::{Value, json};
use support::TestHub;
use support::discord::{FakeDiscord, GUILD, Gw};

const OWNER: &str = "400000000000000001";
const OTHER: &str = "400000000000000002";
const CH: &str = "600000000000000001";
const VOICE_CH: &str = "600000000000000009";
const OWNER_SSRC: u32 = 100;
const OTHER_SSRC: u32 = 200;
const WAIT: Duration = Duration::from_secs(30);

/// One hub with voice on the fakes.
struct Call {
    hub: TestHub,
    discord: FakeDiscord,
    voice: VoiceServer,
    server: realtime::Server,
    _rt: tokio::runtime::Runtime,
    /// Frames of the recording sent so far (so each frame differs).
    n: usize,
}

fn voice_config(base: &str, transport: &str, key: &str, extra: &str) -> String {
    format!(
        r#"[voice]
enabled = true
voice_channel = "{VOICE_CH}"
text_channel = "{CH}"
speech_model = "gpt-live-1"
backend_model = "gpt-5.6-luna"
summary_model = "gpt-6-luna"
voice = "marin"
search_price = 0.01
backend_prices = {{ input = 0.2, cached_input = 0.02, cache_write = 0.25, output = 1.2 }}
summary_prices = {{ input = 0.1, cached_input = 0.01, cache_write = 0.125, output = 0.5 }}
speech_price_per_minute = 0.05
transport = "{transport}"
api_base = "{base}"
key_file = "{key}"
{extra}"#
    )
}

/// The hub's config: the fake OMP lead, Discord on the fake with the guild, and voice.
fn config(hub: &TestHub, discord: &FakeDiscord, voice: &str) -> String {
    let omp = hub.root.join("bin/omp");
    let work = hub.root.join("work");
    let token = hub.root.join("token");
    let discord = discord
        .config(&token, OWNER, &[CH])
        .replace("[discord]\n", &format!("[discord]\nguild = \"{GUILD}\"\n"));
    format!(
        "[omp]\npath = \"{}\"\n[lead]\nmodel = \"fake/primary\"\ncwd = \"{}\"\n{discord}{voice}",
        omp.display(),
        work.display(),
    )
}

impl Call {
    /// Starts the fakes and the hub; voice joins and the owner is present.
    fn start(transport: &str, script: Script, dave: u16, extra: &str) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let server = rt.block_on(realtime::start(script));
        let voice = VoiceServer::start(&rt, dave);
        let discord = FakeDiscord::start();
        discord.channel(CH, 0, None);
        discord.set_voice(
            &voice.endpoint,
            json!({"id": GUILD, "channels": [{"id": VOICE_CH, "type": 2}, {"id": CH, "type": 0}],
                "voice_states": [{"user_id": OWNER, "channel_id": VOICE_CH, "session_id": "o1"}]}),
        );
        let mut hub = TestHub::new("");
        hub.install_fake("omp");
        for f in ["rpc", "turns"] {
            hub.fixture("omp", f, Some(""));
        }
        std::fs::create_dir_all(hub.root.join("work")).unwrap();
        hub.private_file("token", "fake-token");
        let key = hub.private_file("voice-key", "sk-test");
        let text = config(
            &hub,
            &discord,
            &voice_config(&server.base, transport, &key.display().to_string(), extra),
        );
        hub.write_config(&text);
        hub.start();
        let call = Self {
            hub,
            discord,
            voice,
            server,
            _rt: rt,
            n: 0,
        };
        call.hub.until(WAIT, || call.voice.ready().then_some(()));
        call
    }

    fn status(&self) -> Value {
        self.hub.json(&["voice", "status", "--json"])
    }

    fn sessions(&self) -> usize {
        self.server.log.lock().unwrap().sessions.len()
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        self.server.log.lock().unwrap().events_of(kind)
    }

    /// Waits until `f` holds, feeding nothing.
    fn until(&self, limit: Duration, f: impl Fn(&Self) -> bool) -> bool {
        let end = Instant::now() + limit;
        while Instant::now() < end {
            if f(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        f(self)
    }

    /// `ms` of the recording from `ssrc`, in real time.
    fn say(&mut self, ssrc: u32, ms: u32, loud: bool) {
        // As Discord does: the owner is reported speaking while talking, not during room noise.
        if ssrc == OWNER_SSRC {
            self.voice.speaking(OWNER, OWNER_SSRC, loud);
        }
        let frames = opus(ms, loud, self.n);
        self.n += frames.len();
        self.voice.play(ssrc, &frames);
    }

    /// The owner leaves or rejoins the voice channel.
    fn owner_in(&self, inside: bool) {
        let channel = if inside { json!(VOICE_CH) } else { Value::Null };
        self.discord.gw(Gw::Dispatch(
            "VOICE_STATE_UPDATE",
            json!({"guild_id": GUILD, "channel_id": channel, "user_id": OWNER, "session_id": "o1"}),
        ));
    }

    /// 20 ms chunks of session `index`'s input louder than speech level.
    fn loud_chunks(&self, index: usize) -> usize {
        let log = self.server.log.lock().unwrap();
        log.input_bytes.get(index).map_or(0, |b| {
            b.chunks(960)
                .filter(|c| {
                    let s: Vec<f64> = c
                        .chunks_exact(2)
                        .map(|x| f64::from(i16::from_le_bytes([x[0], x[1]])))
                        .collect();
                    (s.iter().map(|v| v * v).sum::<f64>() / s.len().max(1) as f64).sqrt() > 1000.0
                })
                .count()
        })
    }

    fn text_posts(&self) -> Vec<String> {
        self.discord
            .bot_messages(CH)
            .iter()
            .map(|m| m["content"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

macro_rules! both {
    ($name:ident, $body:ident) => {
        mod $name {
            #[test]
            fn websocket() {
                super::$body("websocket");
            }
            #[test]
            fn webrtc() {
                super::$body("webrtc");
            }
        }
    };
}

/// Check 1: with a setting missing voice stays off and names it; a missing key file is named; valid
/// settings start voice, which joins with op 4 on the one Gateway connection, asking for the voice
/// intents.
#[test]
fn check1_settings_name_the_field_and_valid_ones_start() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(realtime::start(Script::default()));
    let discord = FakeDiscord::start();
    discord.channel(CH, 0, None);
    discord.set_voice(
        "127.0.0.1:9",
        json!({"id": GUILD, "channels": [{"id": VOICE_CH, "type": 2}], "voice_states": []}),
    );
    let mut hub = TestHub::new("");
    hub.install_fake("omp");
    hub.fixture("omp", "rpc", Some(""));
    std::fs::create_dir_all(hub.root.join("work")).unwrap();
    hub.private_file("token", "fake-token");
    let missing = voice_config(&server.base, "websocket", "/nonexistent/voice-key", "")
        .replace("speech_model = \"gpt-live-1\"\n", "");
    hub.write_config(&config(&hub, &discord, &missing));
    hub.start();
    let off = hub.until(WAIT, || {
        let s = hub.json(&["voice", "status", "--json"]);
        (s["state"] == "off").then_some(s)
    });
    assert!(
        off["reason"]
            .as_str()
            .unwrap()
            .contains("voice.speech_model"),
        "{off}"
    );
    // Only the key is missing now.
    let no_key = voice_config(&server.base, "websocket", "/nonexistent/voice-key", "");
    hub.write_config(&config(&hub, &discord, &no_key));
    assert!(hub.cli(&["config", "reload"]).status.success());
    let off = hub.until(WAIT, || {
        let s = hub.json(&["voice", "status", "--json"]);
        s["reason"]
            .as_str()
            .is_some_and(|r| r.contains("voice.key_file"))
            .then_some(s)
    });
    assert_eq!(off["state"], "off");
    assert!(discord.voice_ops().is_empty(), "voice never joined");
    // Valid: voice runs and joins neither muted nor deafened.
    let key = hub.private_file("voice-key", "sk-test");
    let valid = voice_config(&server.base, "websocket", &key.display().to_string(), "");
    hub.write_config(&config(&hub, &discord, &valid));
    assert!(hub.cli(&["config", "reload"]).status.success());
    hub.until(WAIT, || {
        (hub.json(&["voice", "status", "--json"])["state"] == "running").then_some(())
    });
    let op = hub.until(WAIT, || discord.voice_ops().first().cloned());
    assert_eq!(op["channel_id"], VOICE_CH);
    assert_eq!(op["self_mute"], false);
    assert_eq!(op["self_deaf"], false);
    let intents = *discord.intents().last().unwrap();
    assert_eq!(
        intents & (1 | 1 << 7),
        1 | 1 << 7,
        "guilds and voice states"
    );
    hub.stop();
}

/// Check 1: a corrupt ledger stops voice through the hub and is never rewritten.
#[test]
fn check1_corrupt_ledger_stops_voice() {
    let mut call = Call::start("websocket", Script::default(), 0, "");
    call.until(WAIT, |c| c.sessions() == 1);
    call.hub.stop();
    let db = call.hub.state().join("cataly.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let day = jiff::Zoned::now()
        .with_time_zone(jiff::tz::TimeZone::UTC)
        .date()
        .to_string();
    conn.execute(
        "INSERT INTO coord (kind, id, version, at_ms, doc) VALUES ('voice_day', ?1, 9, 0, 'garbage')
         ON CONFLICT (kind, id) DO UPDATE SET doc = 'garbage', version = 9",
        [&day],
    )
    .unwrap();
    drop(conn);
    call.hub.start();
    let s = call.hub.until(WAIT, || {
        let s = call.status();
        (s["state"] == "stopped" || s["held"].is_string()).then_some(s)
    });
    assert!(s.to_string().contains("damaged"), "{s}");
    let sessions = call.sessions();
    call.say(OWNER_SSRC, 600, true);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(call.sessions(), sessions, "no session on a corrupt ledger");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let doc: String = conn
        .query_row(
            "SELECT doc FROM coord WHERE kind = 'voice_day' AND id = ?1",
            [&day],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(doc, "garbage", "never reset");
    call.hub.stop();
}

/// Check 2: only the owner's audio is received; a dead receive stream is resubscribed.
fn owner_only_and_resubscribe(t: &str) {
    let mut call = Call::start(t, Script::default(), 0, "");
    assert!(
        call.until(WAIT, |c| c.sessions() == 1),
        "presence opens a session"
    );
    call.voice.speaking(OTHER, OTHER_SSRC, true);
    call.voice.speaking(OWNER, OWNER_SSRC, true);
    std::thread::sleep(Duration::from_millis(200));
    call.say(OTHER_SSRC, 1000, true);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        call.loud_chunks(0),
        0,
        "another speaker never reaches the model"
    );
    call.say(OWNER_SSRC, 1000, true);
    assert!(call.until(Duration::from_secs(3), |c| c.loud_chunks(0) >= 20));
    let wants = call.voice.wants();
    assert!(!wants.is_empty());
    assert!(
        wants
            .iter()
            .all(|w| w.get(OWNER_SSRC.to_string()).is_some()
                && w.get(OTHER_SSRC.to_string()).is_none()),
        "subscribed to the owner only: {wants:?}"
    );
    // 50 undecodable frames in a row end the stream; it comes back and the owner is heard again.
    call.voice
        .play(OWNER_SSRC, &vec![vec![0xff, 0xff, 0xff]; 60]);
    let before = call.loud_chunks(0);
    call.say(OWNER_SSRC, 1000, true);
    assert!(
        call.until(Duration::from_secs(3), |c| c.loud_chunks(0) >= before + 20),
        "resubscribed after a dead stream"
    );
    call.hub.stop();
}
both!(
    check2_owner_only_and_resubscribe,
    owner_only_and_resubscribe
);

/// Check 2: with DAVE negotiated but no group formed, unencrypted owner frames are accepted only
/// within 30 s of the encryption change.
fn plaintext_window(t: &str) {
    let mut call = Call::start(t, Script::default(), 1, "idle_close_seconds = 2\n");
    let started = Instant::now();
    assert_eq!(call.voice.identifies()[0]["max_dave_protocol_version"], 1);
    assert!(
        call.until(WAIT, |c| c.voice.key_packages() >= 1),
        "DAVE key package sent"
    );
    call.voice.speaking(OWNER, OWNER_SSRC, true);
    call.until(WAIT, |c| c.sessions() == 1);
    call.say(OWNER_SSRC, 600, true);
    assert!(
        call.until(Duration::from_secs(3), |c| c.loud_chunks(0) > 0),
        "inside the window"
    );
    // Past the window: the owner's plaintext is dropped, so speech wakes no new session.
    assert!(call.until(WAIT, |c| !c.events("session.close").is_empty()));
    let left = Duration::from_secs(31).saturating_sub(started.elapsed());
    std::thread::sleep(left);
    let sessions = call.sessions();
    call.say(OWNER_SSRC, 1000, true);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(call.sessions(), sessions, "plaintext dropped after 30 s");
    call.hub.stop();
}
both!(check2_plaintext_window, plaintext_window);

/// Check 3: wake on speech sends the 2.5 s lead-in first without a greeting; input is paced at
/// 1.15x real time.
fn wake_and_pace(t: &str) {
    let script = Script {
        greet_ms: 200,
        ..Script::default()
    };
    let mut call = Call::start(t, script, 0, "idle_close_seconds = 2\n");
    call.voice.speaking(OWNER, OWNER_SSRC, true);
    assert!(call.until(WAIT, |c| c.sessions() == 1));
    assert!(call.until(WAIT, |c| !c.events("session.close").is_empty()));
    call.say(OWNER_SSRC, 3000, false);
    call.say(OWNER_SSRC, 400, true);
    assert!(call.until(Duration::from_secs(10), |c| c.sessions() == 2));
    call.say(OWNER_SSRC, 1500, true);
    call.say(OWNER_SSRC, 1000, false);
    let log = call.server.log.lock().unwrap();
    assert!(log.sessions[0].get("input").is_some(), "presence greets");
    assert!(
        log.sessions[1].get("input").is_none(),
        "wake does not greet"
    );
    let received = &log.input_bytes[1];
    let rms = |b: &[u8]| {
        let s: Vec<f64> = b
            .chunks_exact(2)
            .map(|x| f64::from(i16::from_le_bytes([x[0], x[1]])))
            .collect();
        (s.iter().map(|v| v * v).sum::<f64>() / s.len() as f64).sqrt()
    };
    assert!(received.len() > 120_000);
    assert!(rms(&received[..90_000]) < 300.0, "lead-in starts quiet");
    assert!(
        rms(&received[110_000..120_000]) > 1_000.0,
        "lead-in ends with the speech"
    );
    let arrivals: Vec<(Instant, usize)> = log
        .input
        .iter()
        .filter(|(i, _, _)| *i == 1)
        .map(|(_, at, n)| (*at, *n))
        .collect();
    let first = arrivals[0].0;
    let slack = if t == "websocket" { 4_000.0 } else { 9_600.0 };
    let mut total = 0usize;
    for (at, n) in &arrivals {
        total += n;
        let elapsed = at.duration_since(first).as_secs_f64();
        assert!(
            (total as f64) <= 144_000.0 + 1.15 * 48_000.0 * elapsed + slack,
            "paced: {total} bytes after {elapsed:.3}s"
        );
    }
    drop(log);
    call.hub.stop();
}
both!(check3_wake_and_pace, wake_and_pace);

/// Check 3: barge-in clears playback. Measures speech end to the first audio packet at Discord.
fn barge_in(t: &str) {
    let script = Script {
        reply_ms: 3000,
        reply_once: true,
        ..Script::default()
    };
    let mut call = Call::start(t, script, 0, "");
    call.voice.speaking(OWNER, OWNER_SSRC, true);
    assert!(call.until(WAIT, |c| c.sessions() == 1));
    let quiet_out = call.voice.out().len();
    call.say(OWNER_SSRC, 600, true);
    let speech_end = Instant::now();
    assert!(call.until(Duration::from_secs(10), |c| c.voice.out().len() > quiet_out));
    let first = call.voice.out()[quiet_out];
    eprintln!(
        "MEASURE hub {t} speech end to first audio out at Discord: {} ms",
        first.duration_since(speech_end).as_millis()
    );
    call.say(OWNER_SSRC, 500, true);
    call.say(OWNER_SSRC, 3500, false);
    let played = call.voice.out().len() - quiet_out;
    assert!(
        played < 140,
        "barge-in cleared the 3 s reply; {played} packets"
    );
    call.hub.stop();
}
both!(check3_barge_in, barge_in);

/// Checks 4 and 8: pass-to-lead through the real lead reports exactly one of three outcomes and
/// never resends; both transports give the same outcomes, ledger counts and lead deliveries.
fn pass_outcomes(t: &str) -> Vec<String> {
    let script = Script {
        reply_ms: 200,
        tools: vec![(
            "pass_to_lead".into(),
            r#"{"request":"check the `build` $now","high_risk":false}"#.into(),
        )],
        ..Script::default()
    };
    let mut call = Call::start(t, script, 0, "idle_close_seconds = 2\n");
    call.voice.speaking(OWNER, OWNER_SSRC, true);
    let live = call.until(Duration::from_secs(60), |c| {
        c.hub.json(&["lead", "status", "--json"])["phase"] == "live"
    });
    assert!(live, "{}", call.hub.json(&["lead", "status", "--json"]));
    let outputs = |c: &Call| -> Vec<String> {
        c.events("response.item.create")
            .iter()
            .filter_map(|v| v["item"]["output"].as_str().map(str::to_string))
            .filter(|o| o.contains("delivered"))
            .collect()
    };
    assert!(
        call.until(WAIT, |c| c.sessions() == 1),
        "presence opens a session"
    );
    let session_file = call.hub.json(&["lead", "status", "--json"])["session"]["session_file"]
        .as_str()
        .unwrap()
        .to_string();
    let mut done = Vec::new();
    // Received, then unproven (OMP writes no entry), then refused (the lead stopped). Each step is
    // the first owner turn of a session; the next waits for the idle close.
    for step in 0..3 {
        if step == 1 {
            call.hub.fixture("omp", "noentry", Some(""));
        }
        if step == 2 {
            assert!(call.hub.cli(&["lead", "stop", "--force"]).status.success());
        }
        call.say(OWNER_SSRC, 600, true);
        let got = call.until(Duration::from_secs(25), |c| outputs(c).len() > step);
        assert!(
            got,
            "outcome {step}: sessions {}, outputs {:?}, voice {}, lead {}, kinds {:?}",
            call.sessions(),
            outputs(&call),
            call.status(),
            call.hub.json(&["lead", "status", "--json"]),
            call.server
                .log
                .lock()
                .unwrap()
                .events
                .iter()
                .map(|(i, v)| format!("{i}:{}", v["type"]))
                .collect::<Vec<_>>()
        );
        done = outputs(&call);
        call.say(OWNER_SSRC, 500, false);
        assert!(
            call.until(WAIT, |c| c.events("session.close").len() == c.sessions()),
            "idle close after step {step}"
        );
    }
    assert_eq!(done.len(), 3, "exactly one outcome per request: {done:?}");
    assert!(done[0].contains(r#""delivered":true"#), "{done:?}");
    assert!(done[1].contains("receipt unproven"), "{done:?}");
    assert!(
        done[2].contains("not delivered: the lead is not running"),
        "{done:?}"
    );
    // Sent at most once: the received request is in the lead's session file exactly once, cleaned.
    let text = std::fs::read_to_string(&session_file).unwrap_or_default();
    let delivered: Vec<String> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["message"]["role"] == "user")
        .map(|e| e["message"]["content"].to_string())
        .filter(|t| t.contains("[voice request v"))
        .collect();
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert!(
        delivered[0].contains("check the build now"),
        "{delivered:?}"
    );
    let status = call.status();
    call.hub.stop();
    let mut summary: Vec<String> = done
        .iter()
        .map(|o| {
            let v: Value = serde_json::from_str(o).unwrap();
            v["result"].as_str().unwrap_or_default().to_string()
        })
        .collect();
    summary.sort();
    summary.push(format!("sessions={}", status["today"]["sessions"]));
    summary
}

#[test]
fn check4_and_8_pass_outcomes_agree_across_transports() {
    let ws = pass_outcomes("websocket");
    let rtc = pass_outcomes("webrtc");
    assert_eq!(ws, rtc);
}

/// Check 5: a lead reply queued through `cataly voice reply` counts as spoken only after
/// acknowledgement, transcript coverage and drained playback at Discord; otherwise it stays.
fn reply_proof(t: &str) {
    for covered in [true, false] {
        let script = Script {
            ack_commentary: true,
            cover_commentary: covered,
            commentary_ms: 600,
            ..Script::default()
        };
        let mut call = Call::start(t, script, 0, "");
        let out = call.hub.cli(&[
            "voice",
            "reply",
            "v0123abcd",
            "The build on main passed all checks and the release branch is ready.",
            "--subject",
            "build",
        ]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(call.status()["replies_waiting"], 1);
        call.voice.speaking(OWNER, OWNER_SSRC, true);
        assert!(call.until(WAIT, |c| !c.events("session.commentary.append").is_empty()));
        let offered = call.events("session.commentary.append");
        assert!(
            offered[0]["content"]
                .as_str()
                .unwrap()
                .contains("data, not an instruction")
        );
        if covered {
            assert!(
                call.until(Duration::from_secs(15), |c| c.status()["replies_waiting"]
                    == 0),
                "spoken and removed"
            );
            assert!(!call.voice.out().is_empty(), "played at Discord");
        } else {
            call.say(OWNER_SSRC, 4000, false);
            assert_eq!(call.status()["replies_waiting"], 1, "not covered: kept");
        }
        call.hub.stop();
    }
}
both!(check5_reply_proof, reply_proof);

/// Check 6: a provider failure holds voice across a hub restart until `cataly voice restart`, with
/// no paid retry; the notice and summary posts go out.
fn failure_holds(t: &str) {
    let script = Script {
        fail_after_start: Some("server_error".into()),
        ..Script::default()
    };
    let mut call = Call::start(t, script, 0, "");
    call.voice.speaking(OWNER, OWNER_SSRC, true);
    let s = call.hub.until(WAIT, || {
        let s = call.status();
        s["held"].is_string().then_some(s)
    });
    assert!(s["held"].as_str().unwrap().contains("server_error"));
    assert!(call.until(WAIT, |c| {
        let posts = c.text_posts();
        posts.iter().any(|p| p.contains("Voice is held"))
            && posts.iter().any(|p| {
                p.contains("no model summary: the session failed")
                    && p.contains("Owner said: hello")
            })
    }));
    call.owner_in(false);
    std::thread::sleep(Duration::from_millis(300));
    call.owner_in(true);
    call.say(OWNER_SSRC, 600, true);
    call.say(OWNER_SSRC, 1000, false);
    assert_eq!(call.sessions(), 1, "no automatic retry");
    assert!(call.server.log.lock().unwrap().summaries.is_empty());
    // The hold survives a hub restart.
    call.hub.stop();
    call.hub.start();
    call.until(WAIT, |c| c.voice.connections() >= 2);
    let s = call.status();
    assert!(s["held"].as_str().unwrap().contains("server_error"), "{s}");
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(call.sessions(), 1, "held after restart");
    let out = call.hub.cli(&["voice", "restart"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        call.until(WAIT, |c| c.sessions() == 2),
        "restart lifts the hold"
    );
    call.hub.stop();
}
both!(check6_failure_holds, failure_holds);

/// Resident memory (KiB) and CPU time (clock ticks) of a process, from `/proc` (Linux).
fn usage(pid: u32) -> (u64, u64) {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let rss = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    let ticks = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    (rss, ticks)
}

/// Milestone 5 measures (`docs/measurements.md`, "Milestone 5: voice"): the hub's RSS with voice
/// off, then its peak RSS and CPU during a 20 s fake call per transport. Run in a release build on
/// Linux:
/// `cargo test --release --features voice --test voice_hub measure -- --ignored --nocapture`.
#[test]
#[ignore = "measurement; run on demand in a release build"]
fn measure_call_memory_and_cpu() {
    for t in ["websocket", "webrtc"] {
        // Baseline: the same hub and fakes, voice disabled.
        let mut base = Call::start(t, Script::default(), 0, "");
        let text = std::fs::read_to_string(base.hub.config_path())
            .unwrap()
            .replace("enabled = true", "enabled = false");
        base.hub.stop();
        base.hub.write_config(&text);
        base.hub.start();
        std::thread::sleep(Duration::from_secs(8));
        let (idle_rss, _) = usage(base.hub.pid());
        base.hub.stop();
        drop(base);

        let script = Script {
            reply_ms: 1500,
            greet_ms: 500,
            ..Script::default()
        };
        let mut call = Call::start(t, script, 0, "");
        assert!(call.until(WAIT, |c| c.sessions() == 1));
        let pid = call.hub.pid();
        let (_, t0) = usage(pid);
        let started = Instant::now();
        let mut peak = 0;
        let mut gaps = Vec::new();
        for _ in 0..5 {
            call.say(OWNER_SSRC, 1500, true);
            let spoke = Instant::now();
            call.say(OWNER_SSRC, 2500, false);
            if let Some(first) = call.voice.out().into_iter().find(|o| *o > spoke) {
                gaps.push(first.duration_since(spoke).as_millis());
            }
            peak = peak.max(usage(pid).0);
        }
        gaps.sort_unstable();
        let (_, t1) = usage(pid);
        let secs = started.elapsed().as_secs_f64();
        let cpu = (t1 - t0) as f64 / 100.0 / secs * 100.0;
        eprintln!(
            "MEASURE hub {t}: RSS voice off {idle_rss} KiB, peak during call {peak} KiB, added {} KiB; \
             CPU {cpu:.1} % of one core over {secs:.0} s; {} packets out; speech end to first audio \
             out {gaps:?} ms",
            peak.saturating_sub(idle_rss),
            call.voice.out().len()
        );
        call.hub.stop();
    }
}
