//! The ops room against a Discord stand-in that records every create, edit and pin.

use super::*;
use crate::store::tests::open_temp;

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<(String, String)>>,
    next: Mutex<u64>,
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
    fn board_pin<'a>(&'a self, _c: &'a str, m: &'a str) -> BoxFut<'a, ()> {
        self.calls
            .lock()
            .unwrap()
            .push((format!("pin {m}"), String::new()));
        Box::pin(async {})
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
    _keep: (crate::store::WriterHandle, crate::sys::fs::tests::Scratch),
}

impl Room {
    fn new(tag: &str) -> Self {
        let (store, handle, dir) = open_temp(tag);
        Self {
            store,
            fake: Fake::default(),
            settings: OpsSettings::default(),
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
        };
        let extra = Extra {
            host: "부하 172%".into(),
            quota_release_ms: Some(now + HOUR_MS),
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
    // Muted: a quota hold opens no post, but the panel still says so.
    let mute = Mute {
        until_ms: Some(START + 2 * HOUR_MS),
    };
    docs::put(&room.store, STATE, MUTE_ID, &mute, None)
        .await
        .unwrap();
    let hold = event(
        "quota.hold",
        "quota",
        START + 90 * MIN,
        Some(r#"{"limits":"anthropic#1/five_hour","level":"85"}"#),
    );
    room.at(START + 90 * MIN, calm(), vec![hold], &[], ko).await;
    assert_eq!(room.fake.count("create"), creates);
    let last = room.fake.calls().last().cloned().unwrap();
    assert!(last.0.starts_with("edit") && last.1.contains("🔕 알림 꺼짐"));
    assert!(last.1.contains("⛔ 사용량 한도: anthropic 새 세션 보류"));
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
