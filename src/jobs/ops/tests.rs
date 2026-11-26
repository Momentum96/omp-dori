//! The ops room against a Discord stand-in that records every create, edit and pin.

use super::*;
use crate::store::tests::open_temp;

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<(String, String)>>,
    next: Mutex<u64>,
    /// Pins still to refuse for lack of the permission.
    refuse: Mutex<u32>,
    /// What Discord shows as pinned.
    shown: Mutex<Option<bool>>,
}

impl Fake {
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
    fn count(&self, what: &str) -> usize {
        self.calls()
            .iter()
            .filter(|(c, _)| c.starts_with(what))
            .count()
    }
}

impl Board for Fake {
    fn board_create<'a>(
        &'a self,
        _c: &'a str,
        text: &'a str,
        _n: &'a str,
    ) -> BoxFut<'a, Result<String, (bool, String)>> {
        let mut next = self.next.lock().unwrap();
        *next += 1;
        let id = format!("70000000000000{:04}", *next);
        self.calls
            .lock()
            .unwrap()
            .push((format!("create {id}"), text.into()));
        Box::pin(async move { Ok(id) })
    }
    fn board_edit<'a>(
        &'a self,
        _c: &'a str,
        m: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, Result<(), String>> {
        self.calls
            .lock()
            .unwrap()
            .push((format!("edit {m}"), text.into()));
        Box::pin(async { Ok(()) })
    }
    fn board_pin<'a>(&'a self, _c: &'a str, m: &'a str) -> BoxFut<'a, Result<(), (bool, String)>> {
        self.calls
            .lock()
            .unwrap()
            .push((format!("pin {m}"), String::new()));
        let mut refuse = self.refuse.lock().unwrap();
        let result = if *refuse > 0 {
            *refuse -= 1;
            Err((true, "403 Missing Permissions (50013)".to_string()))
        } else {
            Ok(())
        };
        Box::pin(async move { result })
    }
    fn board_is_pinned<'a>(&'a self, _c: &'a str, m: &'a str) -> BoxFut<'a, Option<bool>> {
        self.calls
            .lock()
            .unwrap()
            .push((format!("check {m}"), String::new()));
        let shown = *self.shown.lock().unwrap();
        Box::pin(async move { shown })
    }
}

const MIN: i64 = 60_000;
const START: i64 = 1_800_000_000_000;

fn event(kind: &str, job: &str, at: i64, fields: Option<&str>) -> EventRecord {
    EventRecord {
        id: at,
        at_ms: at,
        job: job.into(),
        subject: job.into(),
        kind: kind.into(),
        severity: "warning".into(),
        text: format!("{job} {kind}"),
        body: None,
        body_path: None,
        lane: None,
        thread: None,
        urgent: false,
        dry: false,
        fields: fields.map(str::to_string),
    }
}

fn job(name: &str, failures: i64) -> JobView {
    JobView {
        name: name.into(),
        kind: "backup".into(),
        trigger: "every 30m".into(),
        enabled: true,
        dry_run: false,
        state: "waiting for trigger".into(),
        held: false,
        consumed: false,
        needs_ack: false,
        next_due_ms: None,
        skipped: 0,
        error_streak: failures,
        last_good_ms: None,
        last_error: Some("git push failed".into()),
        destinations: vec!["cataly-ops".into()],
        last_run: None,
    }
}

struct Room {
    store: Store,
    fake: Fake,
    settings: OpsSettings,
    pin: Mutex<PinTrack>,
    _keep: (crate::store::WriterHandle, crate::sys::fs::tests::Scratch),
}

impl Room {
    fn new(tag: &str) -> Self {
        let (store, handle, dir) = open_temp(tag);
        Self {
            store,
            fake: Fake::default(),
            settings: OpsSettings::default(),
            pin: Mutex::default(),
            _keep: (handle, dir),
        }
    }

    async fn at(
        &self,
        now: i64,
        panel: Panel,
        events: Vec<EventRecord>,
        jobs: &[JobView],
        lang: Language,
    ) -> Option<i64> {
        let turn = Turn {
            store: &self.store,
            board: &self.fake,
            settings: &self.settings,
            lang,
            dest: "cataly-ops",
            channel: "100000000000000012",
            now,
            version: "0.1.1",
            pin: &self.pin,
        };
        let extra = Extra {
            host: "부하 172%".into(),
        };
        turn.run(panel, events, jobs, &extra, false).await
    }
}

fn panel_with(load: f64, state: HostState) -> Panel {
    let mut p = render::tests::sample();
    p.limits = Some(Vec::new());
    p.host.load = Some(load);
    p.host.state = state;
    p
}

/// The panel is created once with both images and pinned, then edited in place on change and at
/// most once a minute.
#[tokio::test]
async fn panel_created_once_then_edited_at_most_once_a_minute() {
    let room = Room::new("ops-panel");
    let ko = Language::Korean;
    let ready = || panel_with(40.0, HostState::Ready);
    room.at(START, ready(), Vec::new(), &[], ko).await;
    let calls = room.fake.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0].0.starts_with("create"));
    assert!(calls[0].1.contains("attachment://panel-banner.jpg"));
    assert!(calls[0].1.contains("attachment://panel-thumb.jpg"));
    assert!(calls[0].1.contains("Cataly 상황판"));
    assert!(calls[1].0.starts_with("pin"), "pinned after the create");
    let id = calls[0].0.trim_start_matches("create ").to_string();
    // Unchanged: nothing; changed within the minute: due later, nothing sent.
    room.at(START + 20_000, ready(), Vec::new(), &[], ko).await;
    let busy = || panel_with(80.0, HostState::Busy);
    let due = room.at(START + 30_000, busy(), Vec::new(), &[], ko).await;
    assert_eq!(due, Some(START + MIN));
    assert_eq!(room.fake.calls().len(), 2);
    room.at(START + MIN, busy(), Vec::new(), &[], ko).await;
    let calls = room.fake.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].0, format!("edit {id}"), "edited, not recreated");
    assert!(calls[2].1.contains("바쁨"));
    assert_eq!(room.fake.count("create"), 1);
}

/// A pin refused for lack of the permission is noted once in the footer and logged once, tried
/// again at most once an hour, and pinned once when the permission arrives.
#[tokio::test]
async fn refused_pin_is_retried_hourly_until_it_works() {
    let room = Room::new("ops-pin-refused");
    let ko = Language::Korean;
    let ready = || panel_with(40.0, HostState::Ready);
    *room.fake.refuse.lock().unwrap() = u32::MAX;
    room.at(START, ready(), Vec::new(), &[], ko).await;
    assert_eq!(room.fake.count("pin"), 1, "tried right after the create");
    // The next edit carries the note; no new try within the hour.
    room.at(START + MIN, ready(), Vec::new(), &[], ko).await;
    let calls = room.fake.calls();
    let edit = calls.last().unwrap();
    assert!(edit.0.starts_with("edit"), "{calls:?}");
    assert_eq!(
        edit.1
            .matches("고정 안 됨: 봇에 메시지 고정 권한이 없어요")
            .count(),
        1
    );
    for minute in 2..60 {
        room.at(START + minute * MIN, ready(), Vec::new(), &[], ko)
            .await;
    }
    assert_eq!(room.fake.count("pin"), 1);
    // Still refused an hour on: tried once more, logged no more.
    room.at(START + HOUR_MS, ready(), Vec::new(), &[], ko).await;
    assert_eq!(room.fake.count("pin"), 2);
    assert_eq!(room.pin.lock().unwrap().refusals_logged, 1);
    // The permission arrives: the next hourly try pins, and the note goes.
    *room.fake.refuse.lock().unwrap() = 0;
    room.at(START + HOUR_MS + 30 * MIN, ready(), Vec::new(), &[], ko)
        .await;
    assert_eq!(room.fake.count("pin"), 2, "not before the hour");
    room.at(START + 2 * HOUR_MS, ready(), Vec::new(), &[], ko)
        .await;
    assert_eq!(room.fake.count("pin"), 3);
    room.at(START + 2 * HOUR_MS + MIN, ready(), Vec::new(), &[], ko)
        .await;
    let calls = room.fake.calls();
    let edit = calls.last().unwrap();
    assert!(edit.0.starts_with("edit"), "{calls:?}");
    assert!(!edit.1.contains("고정 안 됨"));
    for hour in 3..6 {
        room.at(START + hour * HOUR_MS, ready(), Vec::new(), &[], ko)
            .await;
    }
    assert_eq!(room.fake.count("pin"), 3, "pinned once, then left alone");
    assert_eq!(room.pin.lock().unwrap().refusals_logged, 1);
}

/// After a pin works, an unpin by hand ends the tries for the hub run.
#[tokio::test]
async fn hand_unpin_stops_the_tries() {
    let room = Room::new("ops-pin-unpinned");
    let ko = Language::Korean;
    let ready = || panel_with(40.0, HostState::Ready);
    *room.fake.shown.lock().unwrap() = Some(true);
    room.at(START, ready(), Vec::new(), &[], ko).await;
    assert_eq!(room.fake.count("pin"), 1);
    room.at(START + HOUR_MS, ready(), Vec::new(), &[], ko).await;
    assert_eq!(room.fake.count("check"), 1, "still pinned");
    *room.fake.shown.lock().unwrap() = Some(false);
    room.at(START + 2 * HOUR_MS, ready(), Vec::new(), &[], ko)
        .await;
    assert_eq!(room.fake.count("check"), 2);
    for hour in 3..8 {
        room.at(START + hour * HOUR_MS, ready(), Vec::new(), &[], ko)
            .await;
    }
    assert_eq!(room.fake.count("pin"), 1, "never pinned again");
    assert_eq!(room.fake.count("check"), 2, "not watched any more");
    let calls = room.fake.calls();
    assert!(calls.iter().all(|(_, body)| !body.contains("고정 안 됨")));
}

/// Busy and ready flapping for an hour changes only the panel: no alert is ever posted.
#[tokio::test]
async fn flapping_load_posts_nothing() {
    let room = Room::new("ops-flap");
    for minute in 0..60 {
        let (kind, load, state) = if minute % 2 == 0 {
            ("pressure.busy", 91.0, HostState::Busy)
        } else {
            ("pressure.ready", 61.0, HostState::Ready)
        };
        let now = START + minute * MIN;
        let e = event(kind, "host", now, Some(&format!(r#"{{"load":"{load}"}}"#)));
        room.at(now, panel_with(load, state), vec![e], &[], Language::Korean)
            .await;
    }
    assert_eq!(room.fake.count("create"), 1, "only the panel itself");
    assert!(room.fake.count("edit") <= 60);
    assert!(
        room.fake
            .calls()
            .iter()
            .all(|(_, t)| !t.contains("host busy") && !t.contains("pressure.")),
        "no raw event lines"
    );
}

/// Five minutes over the alert level make one alert, edited to resolved when it clears; a second
/// one within the hour is not posted; muted alerts are not posted either.
#[tokio::test]
async fn pressure_alert_resolves_in_place() {
    let room = Room::new("ops-alert");
    let ko = Language::Korean;
    let hot = || panel_with(172.0, HostState::Alert);
    let alert = event(
        "pressure.alert",
        "host",
        START,
        Some(r#"{"reasons":"load"}"#),
    );
    room.at(START, hot(), vec![alert.clone()], &[], ko).await;
    let calls = room.fake.calls();
    let posts: Vec<&(String, String)> = calls
        .iter()
        .filter(|(c, t)| c.starts_with("create") && !t.contains("Cataly 상황판"))
        .collect();
    assert_eq!(posts.len(), 1, "{calls:?}");
    let body: Value = serde_json::from_str(&posts[0].1).unwrap();
    let embed = &body["embeds"][0];
    assert_eq!(embed["title"], "🔥 서버 부하가 5분 넘게 높아요");
    assert_eq!(embed["description"], "부하 172%");
    assert_eq!(embed["thumbnail"]["url"], "attachment://panel-thumb.jpg");
    assert!(embed.get("image").is_none(), "alerts are small");
    let since = format!("<t:{}:t>", (START - ALERT_AFTER_MS) / 1000);
    assert!(
        embed["fields"][0]["value"]
            .as_str()
            .unwrap()
            .contains(&since)
    );
    let alert_id = posts[0].0.trim_start_matches("create ").to_string();
    // The same alert again changes nothing.
    room.at(START + MIN, hot(), vec![alert], &[], ko).await;
    let calm = || panel_with(40.0, HostState::Ready);
    let clear = event("pressure.clear", "host", START + 9 * MIN, None);
    room.at(START + 9 * MIN, calm(), vec![clear], &[], ko).await;
    let edits: Vec<(String, String)> = room
        .fake
        .calls()
        .into_iter()
        .filter(|(c, _)| *c == format!("edit {alert_id}"))
        .collect();
    assert_eq!(edits.len(), 1);
    let body: Value = serde_json::from_str(&edits[0].1).unwrap();
    let title = body["embeds"][0]["title"].as_str().unwrap();
    assert!(title.starts_with("✅ 해결됨 · 🔥"), "{title}");
    let resolved_at = format!("<t:{}:t>", (START + 9 * MIN) / 1000);
    assert!(body.to_string().contains(&resolved_at));
    let creates = room.fake.count("create");
    // Within the hour a new pressure alert is not posted.
    let again = event("pressure.alert", "host", START + 20 * MIN, None);
    room.at(START + 20 * MIN, hot(), vec![again], &[], ko).await;
    assert_eq!(room.fake.count("create"), creates);
    // Muted: a usage alert opens no post, but the panel still says so.
    let mute = Mute {
        until_ms: Some(START + 2 * HOUR_MS),
    };
    docs::put(&room.store, STATE, MUTE_ID, &mute, None)
        .await
        .unwrap();
    let level = event(
        "quota.level",
        "anthropic#1/five_hour",
        START + 90 * MIN,
        Some(r#"{"provider":"anthropic","percent":"85"}"#),
    );
    room.at(START + 90 * MIN, calm(), vec![level], &[], ko)
        .await;
    assert_eq!(room.fake.count("create"), creates);
    let last = room.fake.calls().last().cloned().unwrap();
    assert!(last.0.starts_with("edit") && last.1.contains("🔕 알림 꺼짐"));
    assert!(last.1.contains("🔋 anthropic 사용량 85%"));
}

/// A job routed here that fails twice opens one alert, resolved when it works again; the English
/// variant reads in English.
#[tokio::test]
async fn job_failing_twice_alerts_once_and_resolves() {
    let room = Room::new("ops-jobs");
    let en = Language::English;
    let quiet = || panel_with(40.0, HostState::Ready);
    room.at(START, quiet(), Vec::new(), &[job("home", 1)], en)
        .await;
    assert_eq!(room.fake.count("create"), 1, "one failure is no alert");
    room.at(START + MIN, quiet(), Vec::new(), &[job("home", 2)], en)
        .await;
    room.at(START + 2 * MIN, quiet(), Vec::new(), &[job("home", 3)], en)
        .await;
    let calls = room.fake.calls();
    let alert = calls
        .iter()
        .find(|(c, t)| c.starts_with("create") && t.contains("Job failing: home"))
        .cloned()
        .unwrap();
    assert!(alert.1.contains("Failed 2 times in a row."));
    assert_eq!(room.fake.count("create"), 2);
    room.at(START + 3 * MIN, quiet(), Vec::new(), &[job("home", 0)], en)
        .await;
    let id = alert.0.trim_start_matches("create ");
    assert!(
        room.fake
            .calls()
            .iter()
            .any(|(c, t)| *c == format!("edit {id}") && t.contains("✅ Resolved · "))
    );
    let panel = room
        .fake
        .calls()
        .into_iter()
        .rfind(|(_, t)| t.contains("Cataly status"));
    assert!(panel.is_some_and(|(_, t)| t.contains("🔋 Usage")));
}

/// The shapes stock OMP 19.2.1 printed on the owner's machine (account fields left out): every
/// provider shows by the name OMP reports, the extension provider `devin` included.
const OMP_19_2_1: &str = r#"{"generatedAt":1795660240551,"reports":[
  {"provider":"openai-codex","fetchedAt":1795660096970,"limits":[{"id":"openai-codex:primary","label":"7 days",
    "scope":{"provider":"openai-codex","windowId":"7d","shared":true},
    "window":{"id":"7d","label":"7 days","durationMs":604800000,"resetsAt":1795757995000},
    "amount":{"used":100,"limit":100,"remaining":0,"usedFraction":1,"remainingFraction":0,"unit":"percent"},"status":"exhausted"}]},
  {"provider":"anthropic","fetchedAt":1795660229304,"limits":[
    {"id":"anthropic:5h","label":"Claude 5 Hour","scope":{"provider":"anthropic","windowId":"5h","shared":true},
      "window":{"id":"5h","label":"5 Hour","durationMs":18000000,"resetsAt":1795677600000},
      "amount":{"used":6,"limit":100,"remaining":94,"usedFraction":0.06,"remainingFraction":0.94,"unit":"percent"},"status":"ok"},
    {"id":"anthropic:7d","label":"Claude 7 Day","scope":{"provider":"anthropic","windowId":"7d","shared":true},
      "window":{"id":"7d","label":"7 Day","durationMs":604800000,"resetsAt":1795834800000},
      "amount":{"used":83,"limit":100,"remaining":17,"usedFraction":0.83,"remainingFraction":0.17,"unit":"percent"},"status":"ok"},
    {"id":"anthropic:7d:fable","label":"Claude 7 Day (Fable)","scope":{"provider":"anthropic","windowId":"7d","tier":"fable"},
      "window":{"id":"7d","label":"7 Day","durationMs":604800000,"resetsAt":1795834800000},
      "amount":{"used":20,"limit":100,"remaining":80,"usedFraction":0.2,"remainingFraction":0.8,"unit":"percent"},"status":"ok"}]},
  {"provider":"devin","limits":[{"id":"devin:quota:weekly","label":"Weekly Quota",
    "scope":{"provider":"devin","tier":"Max","windowId":"7d"},
    "window":{"id":"7d","label":"Weekly Quota","durationMs":604800000,"resetsAt":1795939200000},
    "amount":{"used":100,"limit":100,"remaining":0,"usedFraction":1,"remainingFraction":0,"unit":"percent"},"status":"exhausted"}]}],
  "accountsWithoutUsage":[],"disabledCredentials":[],"capacity":{}}"#;

/// The 0.1.3 panel on the owner's machine: the lead live on anthropic, `openai-codex` and `devin`
/// spent.
#[test]
fn the_live_omp_usage_renders_named_providers_and_green() {
    let usage = crate::omp::usage::parse_usage(OMP_19_2_1.as_bytes()).unwrap();
    let limits = worst_per_provider(&usage.limits);
    let shown: Vec<(&str, Option<f64>)> = limits
        .iter()
        .map(|l| (l.provider.as_str(), l.percent))
        .collect();
    assert_eq!(
        shown,
        [
            ("openai-codex", Some(100.0)),
            ("anthropic", Some(83.0)),
            ("devin", Some(100.0)),
        ]
    );
    let mut p = render::tests::sample();
    p.limits = Some(limits);
    p.people = People {
        lead: render::LeadState::Idle,
        model: Some("anthropic/claude-opus-5-5".into()),
        running: 0,
        waiting: 0,
        models: vec!["anthropic/claude-opus-5-5".into()],
    };
    p.host.sessions = p.people.models.len();
    for (lang, words) in [
        (
            Language::Korean,
            [
                "🟢 모두 정상",
                "세션 1/20",
                "**devin** `▰▰▰▰▰▰▰▰` 100% · 리셋 <t:1795939200:R>",
            ],
        ),
        (
            Language::English,
            [
                "🟢 All fine",
                "Sessions 1/20",
                "**devin** `▰▰▰▰▰▰▰▰` 100% · resets <t:1795939200:R>",
            ],
        ),
    ] {
        let body = render::panel(&p, lang, "0.1.4");
        assert_eq!(body["embeds"][0]["color"], 0x002E_CC71, "green");
        let text = body.to_string();
        for want in words {
            assert!(text.contains(want), "{want} in {text}");
        }
        assert!(!text.contains("unknown"), "{text}");
    }
}

#[test]
fn picked_keeps_hidden_channels_a_watched_home_and_watched_others() {
    let id = |n: u64| Snowflake::parse(&(600 + n).to_string()).unwrap();
    let settings = |channels: &[u64], home: Option<u64>| DiscordSettings {
        channels: channels.iter().map(|n| id(*n)).collect(),
        home: home.map(id),
        ..DiscordSettings::default()
    };
    let pick = |channels: &[u64], home: Option<u64>, kept: usize| Picked {
        channels: channels.iter().map(|n| id(*n)).collect(),
        home: home.map(id),
        others: None,
        kept,
    };
    // One channel, home unset: two chosen make the current home explicit.
    assert_eq!(
        picked(&settings(&[1], None), &[id(1), id(2)]),
        pick(&[1, 2], Some(1), 0)
    );
    // The current home dropped: the first chosen channel; duplicates are dropped.
    assert_eq!(
        picked(&settings(&[1, 2], Some(1)), &[id(3), id(2), id(3)]),
        pick(&[3, 2], Some(3), 0)
    );
    // A home still watched stays; one channel left needs no home.
    assert_eq!(
        picked(&settings(&[1, 2], Some(2)), &[id(2)]),
        pick(&[2], None, 0)
    );
    assert_eq!(picked(&settings(&[1], None), &[id(2)]), pick(&[2], None, 0));
    // Channels past the picker's 25 are kept after the chosen ones.
    let many: Vec<u64> = (0..27).collect();
    assert_eq!(
        picked(&settings(&many, Some(0)), &[id(5)]),
        pick(&[5, 25, 26], Some(5), 2)
    );
    // A dropped channel leaves `others`; a watched one stays; untouched `others` is not written.
    let mut now = settings(&[1, 2, 3], Some(1));
    now.others = vec![id(2), id(3)];
    assert_eq!(picked(&now, &[id(1), id(3)]).others, Some(vec![id(3)]));
    assert_eq!(picked(&now, &[id(1), id(2), id(3)]).others, None);
}
