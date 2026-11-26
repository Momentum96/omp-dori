//! Work items and the planner (COORD-19 to COORD-23, COORD-25 to COORD-30; COORD req. 26-41): a
//! queue of later work and an hourly pass that reconciles items with their lanes, audits, starts the
//! next items under the lead, the quota gate, admission and the lane maximum, asks open questions
//! through the lead, runs the owner-notes refresh step and records the run. The planner only opens
//! lanes, emits to the lead and writes its own and the items' records; its GitHub calls are reads.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;

use super::docs::{self, ITEM, PLAN_RUN, PLANNER};
use super::record::{self, Record, State};
use super::{Lanes, need, s};
use crate::host::Runtime;
use crate::jobs::event::{Emit, EventKind, Severity, one_line};
use crate::log;
use crate::sys::now_ms;

const HOUR_MS: i64 = 3_600_000;
const RUNS_KEPT: i64 = 168;
const IMPORT_MAX: usize = 1000;
const REMIND_AFTER_MS: i64 = 6 * HOUR_MS;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Item {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) brief: String,
    pub(crate) cwd: String,
    /// The done checks of the lane it opens (every lane needs one, EVR req. 54).
    pub(crate) checks: Value,
    pub(crate) priority: u8,
    /// `open`, `running`, `done` or `dropped`.
    pub(crate) status: String,
    pub(crate) question: Option<String>,
    pub(crate) asked_ms: Option<i64>,
    pub(crate) reminded_ms: Option<i64>,
    pub(crate) failed_tries: u32,
    pub(crate) start_failures: u32,
    /// `owner/name#number` of an imported issue.
    pub(crate) source: Option<String>,
    pub(crate) created_ms: i64,
    pub(crate) updated_ms: i64,
    pub(crate) log: Vec<String>,
}

impl Item {
    fn note(&mut self, now: i64, text: &str) {
        self.updated_ms = now;
        self.log
            .push(format!("{} {text}", crate::jobs::instant_text(now)));
    }

    fn active(&self) -> bool {
        self.status == "open" || self.status == "running"
    }
}

/// The planner's own state (COORD req. 4, 35).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Planner {
    pub(crate) stopped: Option<String>,
    pub(crate) last_run_ms: Option<i64>,
    /// The local date whose owner-notes refresh was queued (COORD req. 49).
    pub(crate) notes_date: Option<String>,
}

/// One recorded pass (COORD req. 34).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Run {
    pub(crate) at_ms: i64,
    pub(crate) open_lanes: usize,
    pub(crate) max_open: u32,
    pub(crate) unplanned_lanes: usize,
    pub(crate) items_open: usize,
    pub(crate) items_question: usize,
    pub(crate) items_running: usize,
    pub(crate) host: String,
    pub(crate) reconciled: Vec<String>,
    pub(crate) noticed: Vec<String>,
    pub(crate) started: Vec<String>,
    pub(crate) held: Vec<String>,
    pub(crate) asked: Vec<String>,
    pub(crate) duration_ms: i64,
}

impl Run {
    fn summary(&self) -> String {
        let mut text = format!(
            "[planner] {} open lane(s) of {}, {} not from items; items: {} open, {} with a question, {} running",
            self.open_lanes,
            self.max_open,
            self.unplanned_lanes,
            self.items_open,
            self.items_question,
            self.items_running
        );
        for (name, list) in [
            ("reconciled", &self.reconciled),
            ("noticed", &self.noticed),
            ("started", &self.started),
            ("held back", &self.held),
            ("asked", &self.asked),
        ] {
            if !list.is_empty() {
                let _ = write!(text, "\n{name}: {}", list.join("; "));
            }
        }
        text
    }
}

/// `owner/name#number` from a GitHub pull-request link in claim evidence.
pub(super) fn pr_link(text: &str) -> Option<(String, u64)> {
    let at = text.find("github.com/")?;
    let rest = text.get(at + "github.com/".len()..)?;
    let mut parts = rest.split('/');
    let (owner, name, kind, number) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let digits: String = number.chars().take_while(char::is_ascii_digit).collect();
    (kind == "pull" && !owner.is_empty() && !name.is_empty())
        .then(|| digits.parse().ok().map(|n| (format!("{owner}/{name}"), n)))
        .flatten()
}

fn item_id(repo: &str, number: u64) -> String {
    let name = repo.rsplit('/').next().unwrap_or(repo).to_ascii_lowercase();
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let suffix = format!("-{number}");
    let keep = 61usize.saturating_sub(suffix.len());
    let base: String = clean
        .trim_start_matches(['.', '_', '-'])
        .chars()
        .take(keep)
        .collect();
    format!("{}{suffix}", if base.is_empty() { "issue" } else { &base })
}

/// An issue's priority from a `p1`..`p9` label, and whether it carries the decision label.
fn labels(issue: &Value, decision: &str) -> (u8, bool) {
    let names: Vec<&str> =
        issue
            .get("labels")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, |l| {
                l.iter()
                    .filter_map(|x| x.get("name").and_then(Value::as_str))
                    .collect()
            });
    let priority = names
        .iter()
        .find_map(|n| n.strip_prefix('p').and_then(|d| d.parse::<u8>().ok()))
        .filter(|p| (1..=9).contains(p))
        .unwrap_or(record::default_priority());
    (priority, names.contains(&decision))
}

impl<R: Runtime> Lanes<R> {
    async fn planner_state(&self) -> Result<(Planner, i64), String> {
        Ok(docs::get::<Planner>(&self.env.store, PLANNER, "state")
            .await?
            .unwrap_or_default())
    }

    async fn item(&self, id: &str) -> Result<(Item, i64), String> {
        docs::get::<Item>(&self.env.store, ITEM, id)
            .await?
            .ok_or_else(|| format!("no item {id}"))
    }

    /// The item operations and planner switches (COORD req. 4, 35-40).
    pub(super) async fn item_op(
        self: &Arc<Self>,
        op: &str,
        input: &Value,
    ) -> Result<Value, String> {
        let text = |t: String| Ok(json!(t));
        match op {
            "item-add" => text(self.item_add(input).await?),
            "items" => {
                let all = input.get("all").and_then(Value::as_bool).unwrap_or(false);
                let (items, damaged) = docs::all::<Item>(&self.env.store, ITEM).await?;
                let items: Vec<Item> = items
                    .into_iter()
                    .map(|(i, _)| i)
                    .filter(|i| all || i.active())
                    .collect();
                Ok(json!({ "items": items, "damaged": damaged }))
            }
            "item-answer" => {
                let id = need(input, "id")?;
                let answer = need(input, "text")?;
                let (mut item, v) = self.item(id).await?;
                if item.status != "open" {
                    return Err(format!("item {id} is {}", item.status));
                }
                let now = now_ms();
                let question = item.question.take().unwrap_or_default();
                let _ = write!(item.brief, "\n\nQuestion: {question}\nAnswer: {answer}");
                (item.asked_ms, item.reminded_ms) = (None, None);
                (item.failed_tries, item.start_failures) = (0, 0);
                item.note(now, "answered");
                docs::put(&self.env.store, ITEM, id, &item, Some(v)).await?;
                text(format!("item {id} answered; it can start again"))
            }
            "item-drop" => {
                let id = need(input, "id")?;
                let reason = need(input, "reason")?;
                let (mut item, v) = self.item(id).await?;
                if item.status == "running" {
                    return Err(format!("item {id} is running; close its lane first"));
                }
                item.status = "dropped".into();
                item.note(now_ms(), &format!("dropped: {reason}"));
                docs::put(&self.env.store, ITEM, id, &item, Some(v)).await?;
                text(format!("item {id} dropped"))
            }
            "item-import" => self.item_import(input).await,
            "plan" => self.plan_pass(true).await.map(|r| json!(r.summary())),
            "plan-status" => {
                let (state, _) = self.planner_state().await?;
                let (runs, _) = docs::all::<Run>(&self.env.store, PLAN_RUN).await?;
                Ok(json!({
                    "stopped": state.stopped,
                    "last_run_ms": state.last_run_ms,
                    "latest": runs.last().map(|(r, _)| r.summary()),
                    "runs": runs.len(),
                }))
            }
            "plan-stop" | "plan-resume" => {
                let (mut state, v) = self.planner_state().await?;
                state.stopped = (op == "plan-stop")
                    .then(|| s(input, "reason").unwrap_or("stopped").to_string());
                docs::put(&self.env.store, PLANNER, "state", &state, Some(v)).await?;
                text(format!(
                    "planner {}",
                    if state.stopped.is_some() {
                        "stopped"
                    } else {
                        "resumed"
                    }
                ))
            }
            other => Err(format!("unknown item op {other}")),
        }
    }

    /// COORD req. 36-37.
    async fn item_add(&self, input: &Value) -> Result<String, String> {
        let id = need(input, "id")?;
        if let Some(e) = record::key_error(id) {
            return Err(e);
        }
        let brief = match (s(input, "brief"), s(input, "brief_file")) {
            (Some(b), None) => b.to_string(),
            (None, Some(f)) => {
                let path = std::path::Path::new(f);
                if !path.is_absolute() {
                    return Err("brief_file must be an absolute path".into());
                }
                std::fs::read_to_string(path).map_err(|e| format!("the brief file: {e}"))?
            }
            _ => return Err("give exactly one of brief and brief_file".into()),
        };
        let priority = match input.get("priority") {
            None | Some(Value::Null) => record::default_priority(),
            Some(v) => v
                .as_u64()
                .and_then(|p| u8::try_from(p).ok())
                .filter(|p| (1..=9).contains(p))
                .ok_or("priority must be 1 to 9")?,
        };
        let checks = input.get("checks").cloned().unwrap_or(Value::Null);
        super::checks::parse(&checks)?;
        let cwd = s(input, "cwd")
            .map(std::path::PathBuf::from)
            .or_else(|| self.cfg().lanes.cwd.clone())
            .ok_or("no working directory: give cwd or set [lanes] cwd")?;
        let cwd = std::fs::canonicalize(&cwd).map_err(|e| format!("{}: {e}", cwd.display()))?;
        let earlier = docs::get::<Item>(&self.env.store, ITEM, id).await?;
        if earlier.as_ref().is_some_and(|(i, _)| i.active()) {
            return Err(format!("item {id} is already open or running"));
        }
        let now = now_ms();
        let mut item = Item {
            id: id.into(),
            title: s(input, "title").unwrap_or(id).to_string(),
            brief,
            cwd: cwd.display().to_string(),
            checks,
            priority,
            status: "open".into(),
            question: None,
            asked_ms: None,
            reminded_ms: None,
            failed_tries: 0,
            start_failures: 0,
            source: None,
            created_ms: now,
            updated_ms: now,
            log: Vec::new(),
        };
        item.note(now, "added");
        let expect = earlier.map_or(Some(0), |(_, v)| Some(v));
        docs::put(&self.env.store, ITEM, id, &item, expect).await?;
        Ok(format!("item {id} added with priority {priority}"))
    }

    /// Imports labelled open issues (COORD req. 40); reads only.
    async fn item_import(&self, input: &Value) -> Result<Value, String> {
        let repo = need(input, "repo")?;
        if repo.split('/').count() != 2 || repo.contains(['#', ' ', '?']) {
            return Err("repo must be owner/name".into());
        }
        let config = self.cfg();
        let label = s(input, "label")
            .unwrap_or(&config.coord.planner.import_label)
            .to_string();
        let checks = input.get("checks").cloned().unwrap_or(Value::Null);
        super::checks::parse(&checks)?;
        let cwd = s(input, "cwd")
            .map(std::path::PathBuf::from)
            .or_else(|| config.lanes.cwd.clone())
            .ok_or("no working directory: give cwd or set [lanes] cwd")?;
        let cwd = std::fs::canonicalize(&cwd).map_err(|e| format!("{}: {e}", cwd.display()))?;
        let mut issues = Vec::new();
        for page in 1..=10 {
            let path =
                format!("repos/{repo}/issues?state=open&labels={label}&per_page=100&page={page}");
            let answer = super::checks::gh(&path, &cwd).await?;
            let batch = answer.as_array().cloned().unwrap_or_default();
            let len = batch.len();
            issues.extend(
                batch
                    .into_iter()
                    .filter(|i| i.get("pull_request").is_none()),
            );
            if len < 100 || issues.len() >= IMPORT_MAX {
                break;
            }
        }
        let mut added = Vec::new();
        let now = now_ms();
        for issue in issues.iter().take(IMPORT_MAX) {
            let Some(number) = issue.get("number").and_then(Value::as_u64) else {
                continue;
            };
            let source = format!("{repo}#{number}");
            let id = item_id(repo, number);
            if let Some((other, _)) = docs::get::<Item>(&self.env.store, ITEM, &id).await? {
                if other.source.as_deref() != Some(source.as_str()) {
                    return Err(format!("item {id} is held by another source"));
                }
                continue;
            }
            let (priority, decision) = labels(issue, &config.coord.planner.decision_label);
            let title = issue
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            let mut item = Item {
                id: id.clone(),
                title: one_line(&title, 200),
                brief: format!(
                    "{title}\n{}\n\n{}",
                    issue
                        .get("html_url")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    issue
                        .get("body")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                ),
                cwd: cwd.display().to_string(),
                checks: checks.clone(),
                priority,
                status: "open".into(),
                question: decision.then(|| {
                    "this issue is labelled as needing a decision; what should be done?".to_string()
                }),
                asked_ms: None,
                reminded_ms: None,
                failed_tries: 0,
                start_failures: 0,
                source: Some(source),
                created_ms: now,
                updated_ms: now,
                log: Vec::new(),
            };
            item.note(now, "imported");
            docs::put(&self.env.store, ITEM, &id, &item, Some(0)).await?;
            added.push(id);
        }
        let warning = (issues.len() >= IMPORT_MAX)
            .then_some("the 1000-issue limit was reached; some may be missing");
        Ok(json!({ "added": added, "warning": warning }))
    }

    /// The planner task (COORD req. 35): once at start (subject to the gate), then at last run plus
    /// the interval.
    pub(crate) async fn plan_loop(
        self: Arc<Self>,
        mut stop: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let mut probed = self.env.omp.report_changes();
        while self.env.omp.report().is_none() {
            tokio::select! {
                _ = probed.changed() => {}
                () = tokio::time::sleep(Duration::from_secs(30)) => {}
                _ = stop.changed() => return Ok(()),
            }
        }
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            let interval = self.cfg().coord.planner.interval;
            let mut wait = interval;
            if self.cfg().coord.planner.enabled {
                match self.plan_pass(false).await {
                    Ok(_) => {}
                    Err(e) => log::warn("planner pass failed", &[("error", &e)]),
                }
                if let Ok((state, _)) = self.planner_state().await
                    && let Some(last) = state.last_run_ms
                {
                    let due = last + i64::try_from(interval.as_millis()).unwrap_or(i64::MAX);
                    let left = due - now_ms();
                    if left > 0 {
                        wait = Duration::from_millis(u64::try_from(left).unwrap_or(0))
                            .max(Duration::from_secs(1));
                    }
                }
            }
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                _ = stop.changed() => {}
            }
        }
    }

    /// One pass (COORD req. 27-34). `force` skips the interval gate, never the stop switch.
    pub(crate) async fn plan_pass(self: &Arc<Self>, force: bool) -> Result<Run, String> {
        let _one = self.planning.lock().await;
        let (state, state_v) = self.planner_state().await?;
        if let Some(why) = &state.stopped {
            return Err(format!("the planner is stopped: {why}"));
        }
        let config = self.cfg();
        let p = &config.coord.planner;
        let began = now_ms();
        let interval = i64::try_from(p.interval.as_millis()).unwrap_or(i64::MAX);
        if !force
            && state
                .last_run_ms
                .is_some_and(|last| began - last < interval)
        {
            return Err("skipped: the last run is less than an interval ago".into());
        }
        let mut run = Run {
            at_ms: began,
            max_open: p.max_open,
            ..Run::default()
        };
        let (items, damaged) = docs::all::<Item>(&self.env.store, ITEM).await?;
        run.noticed.extend(damaged);
        let rows = self
            .env
            .store
            .read(|r| r.lanes(true))
            .await
            .map_err(|e| e.to_string())?;
        let lanes: Vec<Record> = rows
            .iter()
            .filter_map(|r| serde_json::from_str(&r.record).ok())
            .collect();
        let mut items = self.reconcile(items, &mut run, began).await;
        self.audit(&lanes, &mut run, began).await;
        run.open_lanes = lanes.len();
        run.unplanned_lanes = lanes
            .iter()
            .filter(|l| {
                !items
                    .iter()
                    .any(|(i, _)| i.id == l.key && i.status == "running")
            })
            .count();
        self.plan_starts(&mut items, &lanes, &mut run).await;
        self.ask(&mut items, &mut run, began).await;
        let notes = self.notes_step(&state, &mut run);
        run.items_open = items.iter().filter(|(i, _)| i.status == "open").count();
        run.items_question = items
            .iter()
            .filter(|(i, _)| i.question.is_some() && i.active())
            .count();
        run.items_running = items.iter().filter(|(i, _)| i.status == "running").count();
        let sample = crate::jobs::owner::pressure::sample(std::path::Path::new("/")).await;
        run.host = format!(
            "load {} per cent, free memory {} per cent",
            sample
                .load_percent
                .map_or("unknown".into(), |l| format!("{l:.0}")),
            sample
                .memory_free_percent
                .map_or("unknown".into(), |m| format!("{m:.0}"))
        );
        run.duration_ms = now_ms() - began;
        docs::put(
            &self.env.store,
            PLAN_RUN,
            &format!("{began:016}"),
            &run,
            None,
        )
        .await?;
        docs::trim(&self.env.store, PLAN_RUN, RUNS_KEPT).await?;
        let state = Planner {
            last_run_ms: Some(began),
            notes_date: notes.or(state.notes_date),
            ..state
        };
        docs::put(&self.env.store, PLANNER, "state", &state, Some(state_v)).await?;
        // A pass with no items and nothing found says nothing to the lead.
        let empty = run.reconciled.is_empty()
            && run.noticed.is_empty()
            && run.started.is_empty()
            && run.held.is_empty()
            && run.asked.is_empty();
        if empty {
            return Ok(run);
        }
        let needs = !run.asked.is_empty() || !run.held.is_empty();
        let mut emit = Emit::new(EventKind::PlannerRun, Severity::Info, needs, "planner")
            .with("summary", one_line(&run.summary(), 300));
        emit.body = Some(format!(
            "{}\n(Reply only when something here needs you.)",
            run.summary()
        ));
        self.emit(emit);
        Ok(run)
    }

    /// COORD req. 28.
    async fn reconcile(
        &self,
        items: Vec<(Item, i64)>,
        run: &mut Run,
        now: i64,
    ) -> Vec<(Item, i64)> {
        let mut out = Vec::with_capacity(items.len());
        for (mut item, v) in items {
            let before = item.clone();
            if item.status == "running" {
                match self.load(&item.id).await.ok().map(|(r, _)| r) {
                    Some(rec) if rec.state == State::Closed => {
                        item.status = "done".into();
                        item.note(now, "done: its lane closed");
                        run.reconciled
                            .push(format!("{} done (lane closed)", item.id));
                    }
                    Some(rec) if rec.lost.is_none() => {
                        if let Some((repo, n)) =
                            rec.claim.as_ref().and_then(|c| pr_link(&c.evidence))
                        {
                            match super::checks::gh(
                                &format!("repos/{repo}/pulls/{n}"),
                                std::path::Path::new(&rec.cwd),
                            )
                            .await
                            {
                                Ok(pr)
                                    if pr.get("merged").and_then(Value::as_bool) == Some(true) =>
                                {
                                    item.status = "done".into();
                                    item.note(now, &format!("done: {repo}#{n} merged"));
                                    run.reconciled.push(format!(
                                        "{} done ({repo}#{n} merged; lane {} can be closed)",
                                        item.id, rec.key
                                    ));
                                }
                                Ok(_) => {}
                                Err(e) => run.noticed.push(format!("{}: GitHub: {e}", item.id)),
                            }
                        }
                    }
                    _ => {
                        item.status = "open".into();
                        item.failed_tries += 1;
                        item.note(now, "its lane is dead or missing");
                        run.reconciled
                            .push(format!("{} back to open (lane dead or missing)", item.id));
                        if item.failed_tries >= 2 && item.question.is_none() {
                            item.question = Some(format!(
                                "item {} failed {} times: try again, rewrite the brief, or give it up?",
                                item.id, item.failed_tries
                            ));
                        }
                    }
                }
            } else if item.status == "open"
                && let Some((repo, n)) = item.source.as_ref().and_then(|s| s.split_once('#'))
            {
                let path = format!("repos/{repo}/issues/{n}");
                match super::checks::gh(&path, std::path::Path::new(&item.cwd)).await {
                    Ok(issue) if issue.get("state").and_then(Value::as_str) == Some("closed") => {
                        item.status = "dropped".into();
                        item.note(now, "dropped: its issue is closed");
                        run.reconciled
                            .push(format!("{} dropped (issue closed)", item.id));
                    }
                    Ok(_) => {}
                    Err(e) => run.noticed.push(format!("{}: GitHub: {e}", item.id)),
                }
            }
            let v = if item == before {
                v
            } else {
                match docs::put(&self.env.store, ITEM, &item.id, &item, Some(v)).await {
                    Ok(v) => v,
                    Err(e) => {
                        run.noticed.push(format!("{}: {e}", item.id));
                        v
                    }
                }
            };
            out.push((item, v));
        }
        out
    }

    /// COORD req. 29: read-only findings.
    async fn audit(&self, lanes: &[Record], run: &mut Run, now: i64) {
        for rec in lanes {
            if let Some(c) = &rec.claim
                && now - c.at_ms >= 2 * HOUR_MS
            {
                run.noticed.push(format!(
                    "{} claimed completion {}h ago and is still open",
                    rec.key,
                    (now - c.at_ms) / HOUR_MS
                ));
            }
            if matches!(rec.state, State::Running | State::Starting) {
                let (k, r) = (rec.key.clone(), rec.run_id.clone());
                let mark = self
                    .env
                    .store
                    .read(move |x| x.progress_mark(&k, &r))
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(rec.opened_ms)
                    .max(rec.opened_ms);
                if now - mark >= 3 * HOUR_MS {
                    run.noticed.push(format!(
                        "{} has shown no progress for {}h",
                        rec.key,
                        (now - mark) / HOUR_MS
                    ));
                }
            }
        }
    }

    /// COORD req. 30-31.
    async fn plan_starts(
        self: &Arc<Self>,
        items: &mut [(Item, i64)],
        lanes: &[Record],
        run: &mut Run,
    ) {
        let config = self.cfg();
        let p = &config.coord.planner;
        let mut candidates: Vec<Item> = items
            .iter()
            .map(|(i, _)| i)
            .filter(|i| {
                i.status == "open" && i.question.is_none() && !lanes.iter().any(|l| l.key == i.id)
            })
            .cloned()
            .collect();
        candidates.sort_by(|x, y| {
            (x.priority, x.created_ms, &x.id).cmp(&(y.priority, y.created_ms, &y.id))
        });
        let Some(first) = candidates.first() else {
            return;
        };
        let open = u32::try_from(lanes.len()).unwrap_or(u32::MAX);
        let free = p.max_open.saturating_sub(open).min(p.max_starts);
        // Planner starts run on the lanes' configured model.
        let model = config.lanes.model.as_deref().unwrap_or_default();
        let mut why = (self.env.quota_hold)(model).map(|h| h.to_string());
        if why.is_none() {
            if !(self.env.lead_live)() {
                why = Some("no lead".into());
            } else if let Err(e) = self
                .admission(&config, std::path::Path::new(&first.cwd), Some(model))
                .await
            {
                why = Some(e);
            }
        }
        if let Some(why) = why {
            run.held
                .push(format!("{} item(s) held: {why}", candidates.len()));
            return;
        }
        let used = usize::try_from(free).unwrap_or(0).min(candidates.len());
        for cand in candidates.iter().take(used) {
            if let Some((fresh, nv)) = self.start_item(cand, run).await
                && let Some(slot) = items.iter_mut().find(|(i, _)| i.id == fresh.id)
            {
                *slot = (fresh, nv);
            }
        }
        let left = candidates.len().saturating_sub(used);
        if left > 0 {
            run.held.push(format!(
                "{left} candidate(s) left: {} free lane place(s) against the maximum of {}, {} start(s) per run",
                p.max_open.saturating_sub(open),
                p.max_open,
                p.max_starts
            ));
        }
    }

    /// One attempt of one item (COORD req. 31); returns the item as stored after it.
    async fn start_item(self: &Arc<Self>, item: &Item, run: &mut Run) -> Option<(Item, i64)> {
        let open = json!({
            "op": "open",
            "key": item.id,
            "title": item.title,
            "brief": item.brief,
            "cwd": item.cwd,
            "checks": item.checks,
            "priority": item.priority,
        });
        let outcome = self.open(&open).await;
        let now = now_ms();
        // Only the same item, still open and without a question, becomes running.
        let (mut fresh, fv) = match self.item(&item.id).await {
            Ok(f)
                if f.0.created_ms == item.created_ms
                    && f.0.status == "open"
                    && f.0.question.is_none() =>
            {
                f
            }
            _ => {
                run.noticed
                    .push(format!("{} changed during its start", item.id));
                return None;
            }
        };
        match outcome {
            Ok(o) => {
                let line = o
                    .get("outcome")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if line.starts_with("unconfirmed") {
                    fresh.question = Some(format!(
                        "the start of {} is uncertain ({line}); inspect the lane before anything else",
                        fresh.id
                    ));
                    fresh.note(now, &format!("start uncertain: {line}"));
                    run.held.push(format!("{} start uncertain", fresh.id));
                } else {
                    fresh.status = "running".into();
                    fresh.note(now, &format!("lane opened: {line}"));
                    run.started.push(format!("{} ({line})", fresh.id));
                }
            }
            Err(e) => {
                fresh.start_failures += 1;
                fresh.note(now, &format!("start failed: {e}"));
                run.held
                    .push(format!("{} start failed: {}", fresh.id, one_line(&e, 120)));
                if fresh.start_failures >= 2 {
                    let quote: String = e.chars().take(120).collect();
                    fresh.question = Some(format!(
                        "item {} failed to start twice (\"{quote}\"): fix the brief or directory, or give it up?",
                        fresh.id
                    ));
                }
            }
        }
        match docs::put(&self.env.store, ITEM, &fresh.id, &fresh, Some(fv)).await {
            Ok(nv) => Some((fresh, nv)),
            Err(e) => {
                run.noticed.push(format!("{}: {e}", fresh.id));
                None
            }
        }
    }

    /// COORD req. 32: each question once, one reminder after 6 h.
    async fn ask(&self, items: &mut [(Item, i64)], run: &mut Run, now: i64) {
        for (item, v) in items.iter_mut() {
            let Some(question) = item.question.clone().filter(|_| item.status == "open") else {
                continue;
            };
            let remind = match (item.asked_ms, item.reminded_ms) {
                (None, _) => false,
                (Some(at), None) if now - at >= REMIND_AFTER_MS => true,
                _ => continue,
            };
            let line = format!(
                "{}{question} Answer with lane op item-answer (id {}, text) or drop it with item-drop.",
                if remind { "reminder: " } else { "" },
                item.id
            );
            self.emit(
                Emit::new(EventKind::ItemQuestion, Severity::Info, true, &item.id)
                    .with("item", &item.id)
                    .with("question", &line),
            );
            if remind {
                item.reminded_ms = Some(now);
            } else {
                item.asked_ms = Some(now);
            }
            run.asked
                .push(format!("{}: {}", item.id, one_line(&question, 160)));
            match docs::put(&self.env.store, ITEM, &item.id, &*item, Some(*v)).await {
                Ok(nv) => *v = nv,
                Err(e) => run.noticed.push(format!("{}: {e}", item.id)),
            }
        }
    }

    /// COORD req. 49: the owner-notes refresh once per local day; returns the date claimed.
    fn notes_step(&self, state: &Planner, run: &mut Run) -> Option<String> {
        let hour = self.cfg().coord.planner.notes_hour?;
        let notes = crate::jobs::notes::Notes::new(&self.env.state_dir);
        let now = jiff::Zoned::now();
        let today = now.date().to_string();
        if !notes.exists() || !(self.env.lead_live)() || i64::from(now.hour()) < hour {
            return None;
        }
        if state.notes_date.as_deref() == Some(today.as_str()) {
            return None;
        }
        let path = match notes.write_procedure(&today) {
            Ok(p) => p,
            Err(e) => {
                run.noticed.push(format!("owner notes procedure: {e}"));
                return None;
            }
        };
        self.emit(
            Emit::new(EventKind::NotesRefresh, Severity::Info, true, "notes")
                .with("date", &today)
                .with("procedure", path.display()),
        );
        run.noticed
            .push(format!("owner-notes refresh for {today} queued"));
        Some(today)
    }

    /// Changes an open lane's priority (COORD req. 41).
    pub(super) async fn set_priority(&self, key: &str, input: &Value) -> Result<String, String> {
        let priority = input
            .get("priority")
            .and_then(Value::as_u64)
            .and_then(|p| u8::try_from(p).ok())
            .filter(|p| (1..=9).contains(p))
            .ok_or("priority must be 1 to 9")?;
        let _turn = self.turn(key).await?;
        let (mut rec, rev) = self.load(key).await?;
        if !rec.open() {
            return Err(format!("lane {key} is closed"));
        }
        rec.priority = priority;
        self.save(&rec, Some(rev), None).await?;
        Ok(format!("lane {key} has priority {priority}"))
    }

    /// A lane's recorded priority, for slot waiters (COORD req. 43).
    pub(crate) async fn priority_of(&self, key: &str) -> Option<u8> {
        self.load(key).await.ok().map(|(rec, _)| rec.priority)
    }

    /// The owner's notes commands (COORD req. 47-52).
    pub(super) fn notes_op(&self, op: &str, input: &Value) -> Result<Value, String> {
        let notes = crate::jobs::notes::Notes::new(&self.env.state_dir);
        let today = crate::jobs::notes::Notes::today();
        match op {
            "profile-create" => notes.create().map(Value::String),
            "profile-show" => notes.show().map(Value::String),
            "profile-check" => {
                let problems = notes.check_file(today)?;
                if problems.is_empty() {
                    Ok(json!("the check passes"))
                } else {
                    Err(problems.join("\n"))
                }
            }
            "profile-revert" => notes.revert(need(input, "name")?).map(Value::String),
            _ => Ok(json!(notes.list())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_ids() {
        assert_eq!(
            pr_link("see https://github.com/o/r/pull/12#x"),
            Some(("o/r".into(), 12))
        );
        assert_eq!(pr_link("https://github.com/o/r/issues/12"), None);
        assert_eq!(item_id("Owner/My_Repo", 7), "my_repo-7");
        assert!(record::key_error(&item_id("o/.x", 1)).is_none());
        assert!(record::key_error(&item_id("o/", 123_456)).is_none());
    }
}
