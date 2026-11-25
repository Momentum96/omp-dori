//! Lanes (DESIGN 1, 2.2; LANE, EVR, WS oversight and COORD addresses of milestone 3a): one actor per
//! lane key that serialises lifecycle actions, the lane record in the store, the start sequence with
//! the model check and an OMP goal, reports with evidence, done checks, lead-to-lane messages through
//! the lane's inbox, shell rules for the bridge, thread routing and posting, and oversight.
//!
//! Who may do what is decided here by the caller the hub assigned (DESIGN 9.1): the owner and the
//! lead run lifecycle operations; a lane, through its bridge connection, only reports, records steps,
//! claims completion and registers worktrees for itself (LANE req. 1, 53; OPC-57).

pub(crate) mod board;
pub(crate) mod checks;
mod docs;
pub(crate) mod evidence;
mod git;
mod life;
pub(crate) mod oversight;
mod plan;
mod publish;
mod queue;
pub(crate) mod record;
mod review;
pub(crate) mod shell;
pub(crate) mod slots;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedMutexGuard, watch};

use crate::config::Config;
use crate::host::inbox::{HUB_SENDER, Mailbox, Message, ReceiptState};
use crate::host::lead::{Events, Factory};
use crate::host::{Control, Role, Runtime, StartSpec, Status};
use crate::jobs::event::{Emit, EventKind, Severity};
use crate::log;
use crate::omp::Omp;
use crate::omp::launch::Target;
use crate::store::Store;
use crate::store::lanes::{LaneDone, LaneRow, LaneWrite, Revision};
use crate::sys::latency::Latency;
use crate::sys::now_ms;
use record::{OpenInput, Record, State};

/// An action waits this long for its turn on a lane (LANE req. 31, EVR req. 60).
const TURN_WAIT: Duration = Duration::from_secs(30);
/// Transcript entries a read returns by default and at most (LANE req. 33).
const READ_DEFAULT: usize = 20;
const READ_MAX: usize = 40;
/// Answers to a lane's dialog are refused above this size (LANE req. 34).
const ANSWER_MAX: usize = 64 * 1024;
/// Report text in the lead's wake line (EVR req. 17).
const WAKE_TEXT: usize = 500;
/// The look-only tool set (OPC-46), also the reviewer's. OMP 19.1.1 refuses `--tools` naming an
/// unknown tool (`ls`) or one unavailable in the session (`find` needs a native judge model).
const LOOK_ONLY: &str = "read,grep,glob";

pub(crate) type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The outcome of one thread post (MSG-05).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Posted {
    Confirmed,
    Rejected(String),
    Uncertain(String),
}

/// What lanes need from the relay for their threads (DISC-OUT-11 to 13, EVR-14) and the board.
pub(crate) trait Threads: board::Board + Send + Sync {
    /// Posts `text` once (the seed makes the nonce) into a thread.
    fn post<'a>(&'a self, thread: &'a str, text: &'a str, seed: &'a str) -> BoxFut<'a, Posted>;
    /// Puts the status word in front of the thread's title, read fresh; archives when asked.
    fn status<'a>(
        &'a self,
        thread: &'a str,
        word: &'a str,
        archive: bool,
    ) -> BoxFut<'a, Result<(), String>>;
    /// Creates a public thread from an owner message after checking its source (DISC-IN-22).
    /// `Err((uncertain, why))`.
    fn create<'a>(
        &'a self,
        room: &'a str,
        message: &'a str,
        title: &'a str,
    ) -> BoxFut<'a, Result<String, (bool, String)>>;
}

/// What lanes need from the hub.
pub(crate) struct Env<R: Runtime> {
    pub(crate) config: watch::Receiver<Arc<Config>>,
    pub(crate) omp: Arc<Omp>,
    pub(crate) store: Store,
    pub(crate) state_dir: PathBuf,
    pub(crate) events: Events,
    pub(crate) make: Factory<R>,
    pub(crate) gate_held: Arc<dyn Fn() -> bool + Send + Sync>,
    pub(crate) bridge_link: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    /// Whether the lead's session is live (it counts toward the session cap).
    pub(crate) lead_live: Arc<dyn Fn() -> bool + Send + Sync>,
    pub(crate) threads: Arc<dyn Threads>,
    /// Board posts on Telegram and Slack (`telegram:…`, `slack:…` destinations).
    pub(crate) boards: Arc<dyn board::Board>,
}

/// Who calls (DESIGN 9.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Caller {
    Owner,
    Lead,
    /// A lane's bridge connection: its session handle and the session it vouched for.
    Lane {
        handle: String,
        session: String,
    },
}

/// The lanes of this hub.
pub(crate) struct Lanes<R: Runtime> {
    env: Env<R>,
    live: Mutex<HashMap<String, Arc<R::Session>>>,
    turns: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    opening: Mutex<Vec<String>>,
    /// Closes on threads are serialised so two last lanes cannot both skip the hook (LANE req. 42).
    closing: AsyncMutex<()>,
    overlay: OnceLock<PathBuf>,
    spend: Mutex<HashMap<String, crate::omp::transcript::SpendTree>>,
    /// Wakes oversight early (a report, a state change, a claim).
    pub(crate) nudge: Notify,
    /// Spawn to kickoff confirmed (plan milestone 3a measure).
    pub(crate) start_latency: Latency,
    /// Named slots for heavy commands (LANE-10).
    pub(crate) slots: Arc<slots::Slots>,
    /// One planner pass at a time (COORD req. 35).
    planning: AsyncMutex<()>,
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

fn need<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    s(v, key).ok_or_else(|| format!("{key} is required"))
}

/// An objection's reasons: the `reasons` list, or the single `reason`.
fn reasons(input: &Value) -> Result<Vec<String>, String> {
    Ok(match input.get("reasons") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => vec![need(input, "reason")?.to_string()],
    })
}

/// `opened:length`, the revision a caller saw (LANE req. 30, 33).
pub(crate) fn revision_text(r: Revision) -> String {
    format!("{}:{}", r.0, r.1)
}

impl<R: Runtime> Lanes<R> {
    pub(crate) fn new(env: Env<R>) -> Arc<Self> {
        let slots = Arc::new(slots::Slots::new(env.config.clone()));
        Arc::new(Self {
            env,
            live: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            opening: Mutex::new(Vec::new()),
            closing: AsyncMutex::new(()),
            overlay: OnceLock::new(),
            spend: Mutex::new(HashMap::new()),
            nudge: Notify::new(),
            start_latency: Latency::default(),
            slots,
            planning: AsyncMutex::new(()),
        })
    }

    fn cfg(&self) -> Arc<Config> {
        Arc::clone(&self.env.config.borrow())
    }

    fn live_map(&self) -> MutexGuard<'_, HashMap<String, Arc<R::Session>>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn session(&self, key: &str) -> Option<Arc<R::Session>> {
        self.live_map().get(key).filter(|s| s.alive()).cloned()
    }

    /// Live lanes that may not be stopped now, with the reason.
    pub(crate) fn busy(&self) -> Vec<(String, &'static str)> {
        self.live_map()
            .iter()
            .filter(|(_, s)| s.alive())
            .filter_map(|(key, s)| s.snapshot().busy().map(|why| (key.clone(), why)))
            .collect()
    }

    fn mailbox(&self, key: &str) -> Mailbox {
        Mailbox {
            target: key.to_string(),
            store: self.env.store.clone(),
        }
    }

    fn emit(&self, emit: Emit) {
        self.lane_event(emit.kind, &emit.values);
        (self.env.events)(emit);
    }

    fn alert(&self, key: &str, what: &str, detail: &str, urgent: bool) {
        self.emit(
            Emit::new(EventKind::LaneAlert, Severity::Warning, urgent, key)
                .with("lane", key)
                .with("what", what)
                .with("detail", detail),
        );
    }

    /// The lane's turn: actions on one lane run one at a time (LANE req. 31).
    async fn turn(&self, key: &str) -> Result<OwnedMutexGuard<()>, String> {
        let lock = Arc::clone(
            self.turns
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.to_string())
                .or_default(),
        );
        tokio::time::timeout(TURN_WAIT, lock.lock_owned())
            .await
            .map_err(|_| format!("busy: another action on lane {key} holds it"))
    }

    async fn row(&self, key: &str) -> Result<Option<LaneRow>, String> {
        let k = key.to_string();
        self.env
            .store
            .read(move |r| r.lane(&k))
            .await
            .map_err(|e| e.to_string())
    }

    async fn load(&self, key: &str) -> Result<(Record, Revision), String> {
        let row = self
            .row(key)
            .await?
            .ok_or_else(|| format!("no lane {key}"))?;
        let rec: Record = serde_json::from_str(&row.record)
            .map_err(|e| format!("lane {key} record damaged: {e}"))?;
        Ok((rec, (row.opened_ms, row.revision)))
    }

    /// Writes the record; a `note` is a state change with a history entry (LANE req. 30).
    async fn save(
        &self,
        rec: &Record,
        expect: Option<Revision>,
        note: Option<String>,
    ) -> Result<Revision, String> {
        let write = LaneWrite::Save {
            key: rec.key.clone(),
            state: rec.state.name().into(),
            outcome: rec.outcome.clone(),
            thread: rec.thread.clone(),
            opened_ms: rec.opened_ms,
            closed_ms: rec.closed_ms,
            record: serde_json::to_string(rec).map_err(|e| e.to_string())?,
            expect,
            note,
            reopen: false,
        };
        match self
            .env
            .store
            .lanes(write)
            .await
            .map_err(|e| e.to_string())?
        {
            LaneDone::Saved(r) => Ok(r),
            LaneDone::Refused(why) => Err(why),
            other => Err(format!("unexpected store answer {other:?}")),
        }
    }

    /// Writes bookkeeping only (no revision change); a lane that moved on is left alone.
    async fn keep(&self, key: &str, f: impl FnOnce(&mut Record)) {
        let Ok(_turn) = self.turn(key).await else {
            return;
        };
        if let Ok((mut rec, rev)) = self.load(key).await {
            f(&mut rec);
            if let Err(e) = self.save(&rec, Some(rev), None).await {
                log::warn(
                    "lane bookkeeping not saved",
                    &[("lane", &key), ("error", &e)],
                );
            }
        }
    }

    /// The lanes' overlay, written once per hub run (DESIGN 4.2): goal mode on (decision 5).
    fn overlay(&self, config: &Config) -> Result<PathBuf, String> {
        if let Some(path) = self.overlay.get() {
            return Ok(path.clone());
        }
        let path = crate::omp::launch::write_overlay(&self.env.state_dir, &config.fallback, true)
            .map_err(|e| e.to_string())?;
        Ok(self.overlay.get_or_init(|| path).clone())
    }

    /// Admission for every open, reopen and restart (LANE req. 18; OJ-08; C14).
    async fn admission(&self, config: &Config, cwd: &Path) -> Result<PathBuf, String> {
        let report = self.env.omp.report().ok_or("OMP has not been probed yet")?;
        report.sessions()?;
        let exe = self
            .env
            .omp
            .path()
            .ok_or("the OMP executable was not found")?;
        if (self.env.gate_held)() {
            return Err(
                "the quota gate holds new session starts; `cataly quota lift` lifts it".into(),
            );
        }
        (self.env.bridge_link)()?;
        let live = self.live_map().values().filter(|s| s.alive()).count();
        let sessions = live + usize::from((self.env.lead_live)());
        if sessions >= usize::try_from(config.sessions.cap).unwrap_or(usize::MAX) {
            return Err(format!(
                "the session cap of {} is reached",
                config.sessions.cap
            ));
        }
        let max = usize::try_from(config.lanes.max).unwrap_or(usize::MAX);
        if max > 0 && live >= max {
            return Err(format!("{live} live lanes reach the maximum of {max}"));
        }
        let pressure = config
            .jobs
            .values()
            .find_map(|j| match &j.work {
                crate::config::Work::Owner(crate::config::Owner::Pressure(p)) => Some(p.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let sample = crate::jobs::owner::pressure::sample(cwd).await;
        let sample = crate::jobs::owner::pressure::Sample {
            sessions: u32::try_from(live).ok(),
            ..sample
        };
        let held = crate::jobs::owner::pressure::admission(&sample, &pressure);
        if !held.is_empty() {
            return Err(format!("admission holds the start: {}", held.join(", ")));
        }
        self.overlay(config)?;
        Ok(exe)
    }

    /// DESIGN 9.2 for the terminal runtime. A headless lane is fenced when `get_state.usageLimit`
    /// reports extra usage; OMP 19.1.1 gives extensions no view of that state, so the bridge cannot
    /// report it from a tern session. A tern lane therefore starts only while a fresh account-limits
    /// read shows no paid extra usage for its model's provider; an unreadable answer refuses too.
    async fn tern_spend(&self, config: &Config, rec: &Record) -> Result<(), String> {
        if rec.runtime != crate::config::session::RuntimeKind::Tern {
            return Ok(());
        }
        let provider = rec.model.split_once('/').map_or("", |(p, _)| p);
        let omp = &self.env.omp;
        let read = omp
            .usage
            .read(omp.path().as_deref(), config.limits.cache, false)
            .await;
        let snapshot = read.snapshot.filter(|_| !read.stale).ok_or_else(|| {
            format!(
                "a tern lane starts only after the account limits rule out paid extra usage, and \
                 they could not be read ({}); use the headless runtime",
                read.error.as_deref().unwrap_or("stale answer")
            )
        })?;
        if snapshot.usage.extra_usage(provider) {
            return Err(format!(
                "the {provider} account reports paid extra usage, and a tern session cannot be \
                 fenced before a request bills it; use the headless runtime"
            ));
        }
        Ok(())
    }

    /// Runs one operation for a caller; the answer is JSON for the CLI and the tools.
    pub(crate) async fn call(
        self: &Arc<Self>,
        caller: Caller,
        input: &Value,
    ) -> Result<Value, String> {
        let op = s(input, "op").unwrap_or_default();
        if let Caller::Lane { handle, session } = &caller {
            return self.lane_op(op, handle, session, input).await;
        }
        let key = || need(input, "key");
        let by = if caller == Caller::Owner {
            "owner"
        } else {
            "lead"
        };
        let text = |t: String| Ok(json!(t));
        match op {
            "open" => self.open(input).await,
            "list" => {
                self.list(input.get("all").and_then(Value::as_bool).unwrap_or(false))
                    .await
            }
            "board" => self.board(s(input, "key")).await.map(Value::String),
            "history" | "reports" => self.ledger(op, key()?).await,
            "pause" => text(
                self.pause(
                    key()?,
                    by,
                    s(input, "reason").unwrap_or("explicit instruction"),
                )
                .await?,
            ),
            "resume" => text(self.resume(key()?).await?),
            "park" | "abandon" => text(
                self.park(key()?, op, need(input, "reason")?, s(input, "condition"))
                    .await?,
            ),
            "claim" => text(self.claim(key()?, need(input, "evidence")?).await?),
            "object" => text(self.object(key()?, &reasons(input)?).await?),
            "close" => text(
                self.close(
                    key()?,
                    s(input, "note").unwrap_or("closed by the lead"),
                    s(input, "lesson"),
                )
                .await?,
            ),
            "reopen" => text(self.reopen(key()?, s(input, "note")).await?),
            "send" => text(
                self.send(
                    key()?,
                    need(input, "text")?,
                    s(input, "revision"),
                    s(input, "id"),
                    input.get("steer").and_then(Value::as_bool).unwrap_or(false),
                )
                .await?,
            ),
            "release" => text(self.release(key()?, need(input, "note")?).await?),
            "read" => self.read(key()?, input.get("count").and_then(Value::as_u64)),
            "interrupt" => text(
                self.control(key()?, s(input, "revision"), |s| {
                    Box::pin(async move { s.interrupt().await })
                })
                .await?,
            ),
            "model" => {
                let model = need(input, "model")?.to_string();
                text(self.set_model(key()?, s(input, "revision"), &model).await?)
            }
            "answer" => self
                .answer(
                    key()?,
                    need(input, "dialog")?,
                    input.get("answer").unwrap_or(&Value::Null),
                )
                .await
                .map(|()| json!("answered")),
            "grant" => {
                let rule = input.get("rule").and_then(Value::as_u64);
                let rule = rule.ok_or("rule is required")?;
                text(
                    self.grant(
                        key()?,
                        usize::try_from(rule).unwrap_or(0),
                        need(input, "reason")?,
                    )
                    .await?,
                )
            }
            "drift" => self.drift().await,
            "spend" => self.spend_view(s(input, "key")).await,
            other => self.coord_op(&caller, other, input).await,
        }
    }

    /// The coordination operations of milestone 3b for the lead and the owner.
    async fn coord_op(
        self: &Arc<Self>,
        caller: &Caller,
        op: &str,
        input: &Value,
    ) -> Result<Value, String> {
        let key = || need(input, "key");
        let text = |t: String| Ok(json!(t));
        match op {
            "priority" => text(self.set_priority(key()?, input).await?),
            "queued" => self.queued(input).await,
            "start" => self.start_queued(key()?).await,
            "cancel" => text(self.cancel_queued(key()?).await?),
            "item-add" | "items" | "item-answer" | "item-drop" | "item-import" | "plan"
            | "plan-status" | "plan-stop" | "plan-resume" => self.item_op(op, input).await,
            "review" => self.review(input).await,
            "merge" => self.merge(input).await,
            "publish" => self.publish(input, None).await,
            "files" => self.files(s(input, "key")).await,
            "rule-add" | "rule-retire" | "rules" => self.rule_op(op, input, None).await,
            "board-found" => self.board_found(input).await,
            "admission" => {
                let config = self.cfg();
                let cwd = s(input, "cwd")
                    .map(PathBuf::from)
                    .or_else(|| config.lanes.cwd.clone())
                    .unwrap_or_else(|| PathBuf::from("/"));
                Ok(match self.admission(&config, &cwd).await {
                    Ok(_) => json!({ "admit": true, "detail": "a start may go ahead now" }),
                    Err(e) => json!({ "admit": false, "detail": e }),
                })
            }
            "slots" => Ok(json!({ "slots": self.slots.view(), "lines": self.slots.lines() })),
            "profile-create" | "profile-show" | "profile-check" | "profile-revert"
            | "profile-list"
                if *caller == Caller::Owner =>
            {
                self.notes_op(op, input)
            }
            other => Err(format!(
                "unknown lane op '{other}'; ops: open, list, board, history, reports, pause, resume, park, \
                 abandon, claim, object, close, reopen, send, release, read, interrupt, model, answer, grant, \
                 drift, spend, priority, queued, start, cancel, item-add, items, item-answer, item-drop, \
                 item-import, plan, plan-status, plan-stop, plan-resume, review, merge, publish, files, \
                 rule-add, rule-retire, rules, board-found, admission, slots, profile-create, profile-show, \
                 profile-check, profile-revert, profile-list (owner)"
            )),
        }
    }

    /// A lane's history or its last reports, as stored.
    async fn ledger(&self, op: &str, key: &str) -> Result<Value, String> {
        let k = key.to_string();
        let store = &self.env.store;
        let rows = if op == "history" {
            store
                .read(move |r| r.lane_history(&k))
                .await
                .map(|h| json!(h))
        } else {
            store
                .read(move |r| r.reports(&k, None, 50))
                .await
                .map(|h| json!(h))
        };
        rows.map_err(|e| e.to_string())
    }

    /// The lane tool (EVR-01, EVR-20, LANE req. 53, 61): only for the current run's own session.
    async fn lane_op(
        self: &Arc<Self>,
        op: &str,
        handle: &str,
        session: &str,
        input: &Value,
    ) -> Result<Value, String> {
        let key = self
            .key_of(handle)
            .await
            .ok_or(crate::store::lanes::MOVED_ON)?;
        let (rec, _) = self.load(&key).await?;
        let current = rec.open()
            && rec.handle.as_deref() == Some(handle)
            && rec.session_id.as_deref() == Some(session)
            && rec.intent.is_none();
        let run = current.then(|| rec.run_id.clone());
        match op {
            "report" => {
                if !current {
                    return Err(crate::store::lanes::MOVED_ON.into());
                }
                self.report(&rec, input).await.map(Value::String)
            }
            // A step is kept even when the run cannot be established (EVR req. 32).
            "step" => self.step(&rec, run, input).await.map(Value::String),
            "steps" => {
                let k = key.clone();
                let steps = self
                    .env
                    .store
                    .read(move |r| r.steps(&k))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(json!(steps))
            }
            "claim" => {
                if !current {
                    return Err(crate::store::lanes::MOVED_ON.into());
                }
                self.claim(&key, need(input, "evidence")?)
                    .await
                    .map(Value::String)
            }
            "worktree" => {
                if !current {
                    return Err(crate::store::lanes::MOVED_ON.into());
                }
                let path = need(input, "path")?.to_string();
                let real = std::fs::canonicalize(&path).map_err(|e| format!("{path}: {e}"))?;
                let real = real.display().to_string();
                self.keep(&key, |r| {
                    if !r.worktrees.contains(&real) {
                        r.worktrees.push(real.clone());
                    }
                })
                .await;
                Ok(json!(format!("worktree {real} registered for lane {key}")))
            }
            "publish" => {
                if !current {
                    return Err(crate::store::lanes::MOVED_ON.into());
                }
                self.publish(input, Some(&key)).await
            }
            "rule-add" | "rule-retire" | "rules" => self.rule_op(op, input, Some(&rec)).await,
            other => Err(format!(
                "unknown op '{other}'; ops: report, step, steps, claim, worktree, publish, rule-add, \
                 rule-retire, rules"
            )),
        }
    }

    /// The lane whose current run holds this session handle.
    async fn key_of(&self, handle: &str) -> Option<String> {
        let rows = self.env.store.read(|r| r.lanes(false)).await.ok()?;
        rows.into_iter().find_map(|row| {
            let rec: Record = serde_json::from_str(&row.record).ok()?;
            (rec.handle.as_deref() == Some(handle)).then_some(rec.key)
        })
    }

    /// The handle of the open lane whose session this is (a worker's parent, EXT req. 9).
    pub(crate) async fn handle_of_session(&self, session: &str) -> Option<String> {
        if session.is_empty() {
            return None;
        }
        let rows = self.env.store.read(|r| r.lanes(true)).await.ok()?;
        rows.into_iter().find_map(|row| {
            let rec: Record = serde_json::from_str(&row.record).ok()?;
            (rec.session_id.as_deref() == Some(session))
                .then_some(rec.handle)
                .flatten()
        })
    }

    /// The model barrier's judgement for a lane's main agent (EXT req. 38-39). The bridge reports
    /// the model OMP runs, its thinking level and the fallback OMP announced; the pin or that
    /// fallback (decision 7) passes, anything else holds the turn and the lead hears it.
    pub(crate) async fn model_check(&self, handle: &str, facts: &Value) -> (bool, String) {
        let found = match self.key_of(handle).await {
            Some(key) => self.load(&key).await.ok().filter(|(r, _)| r.open()),
            None => None,
        };
        let Some((rec, _)) = found else {
            return (false, "this session belongs to no open lane".into());
        };
        let fact = |k: &str| s(facts, k).unwrap_or_default();
        let (now, fallback) = (fact("model"), fact("fallback"));
        let same = |m: &str| !m.is_empty() && crate::host::dialogs::same_model(now, m);
        // OMP names a fallback with its thinking suffix (`provider/model:level`).
        let fell_back = same(fallback) || fallback.rsplit_once(':').is_some_and(|(m, _)| same(m));
        let thinking_ok = rec
            .thinking
            .as_deref()
            .is_none_or(|t| fact("thinking") == t);
        if (same(&rec.model) || fell_back) && thinking_ok {
            return (true, String::new());
        }
        let pin = rec
            .thinking
            .as_ref()
            .map_or_else(|| rec.model.clone(), |t| format!("{}:{t}", rec.model));
        let on = if now.is_empty() {
            "an unknown model"
        } else {
            now
        };
        let detail = format!(
            "pinned {pin}; OMP is on {on} at thinking {}",
            fact("thinking")
        );
        self.alert(&rec.key, "model hold", &detail, true);
        (
            false,
            format!(
                "Cataly held this turn: {detail}. The lead was told; never switch models yourself."
            ),
        )
    }

    /// The veto for one shell or subagent-spawn call of a lane session (EXT req. 32-34). The
    /// bridge sends the tool, the command and whether the session file exists; any doubt blocks.
    pub(crate) async fn veto(&self, handle: &str, call: &Value) -> (bool, String) {
        let no = |why: &str| {
            (
                false,
                format!("No command ran: {why}. Report a blocker (kind waiting)."),
            )
        };
        match (s(call, "tool"), call.get("command").and_then(Value::as_str)) {
            (Some("task"), _) if call.get("saved").and_then(Value::as_bool) != Some(true) => {
                no("the session is not on disk yet, so a subagent could not inherit its rules")
            }
            (Some("task"), _) => match self.hello(handle).await {
                Some(_) => (true, String::new()),
                None => no("this session belongs to no open lane"),
            },
            (Some("bash"), Some(command)) => self.shell(handle, command).await,
            _ => no("the call has no command string"),
        }
    }

    /// What the bridge of a lane session needs at hello: key, model pin and whether shell rules
    /// apply (EXT req. 4, 38, 32). `None` for a handle no open lane holds.
    pub(crate) async fn hello(&self, handle: &str) -> Option<(String, String)> {
        let key = self.key_of(handle).await?;
        let (rec, _) = self.load(&key).await.ok()?;
        let pin = match &rec.thinking {
            Some(t) => format!("{}:{t}", rec.model),
            None => rec.model.clone(),
        };
        rec.open().then_some((key, pin))
    }

    /// The shell veto for one command of a lane session or its subagents (LANE-30, EXT-31).
    pub(crate) async fn shell(&self, handle: &str, command: &str) -> (bool, String) {
        let Some(key) = self.key_of(handle).await else {
            return (
                false,
                "Blocked: this session belongs to no open lane. No command ran.".into(),
            );
        };
        let Ok(_turn) = self.turn(&key).await else {
            return (
                false,
                "Blocked: the lane is busy; no command ran. Retry shortly.".into(),
            );
        };
        let Ok((mut rec, rev)) = self.load(&key).await else {
            return (
                false,
                "Blocked: the lane's shell rules are unreadable. No command ran.".into(),
            );
        };
        // A closed lane's run holds no shell rights; its handle may outlive it (fail closed).
        if !rec.open() || rec.handle.as_deref() != Some(handle) {
            return (
                false,
                "Blocked: this session belongs to no open lane. No command ran.".into(),
            );
        }
        let compile = |list: &[String]| {
            list.iter()
                .map(|p| regex::Regex::new(p))
                .collect::<Result<Vec<_>, _>>()
        };
        let (Ok(deny), Ok(allow)) = (compile(&rec.deny), compile(&rec.allow)) else {
            return (
                false,
                "Blocked: the lane's shell rules are unreadable. No command ran.".into(),
            );
        };
        let verdict = shell::judge(command, &deny, &allow, &mut rec.grants);
        if verdict.allowed && verdict.used.is_empty() {
            return (true, String::new());
        }
        if !verdict.allowed {
            rec.last_block = Some((verdict.digest.clone(), verdict.rules.clone()));
        }
        if let Err(e) = self.save(&rec, Some(rev), None).await {
            return (
                false,
                format!("Blocked: the verdict was not recorded ({e}). No command ran."),
            );
        }
        if verdict.allowed {
            log::info(
                "lane shell grant used",
                &[("lane", &key), ("rules", &format!("{:?}", verdict.used))],
            );
            (true, String::new())
        } else {
            (false, shell::blocker(&verdict))
        }
    }

    /// One attempt for one rule of the lane's most recently blocked command (LANE req. 75).
    async fn grant(&self, key: &str, rule: usize, reason: &str) -> Result<String, String> {
        let _turn = self.turn(key).await?;
        let (mut rec, rev) = self.load(key).await?;
        let (digest, rules) = rec
            .last_block
            .clone()
            .ok_or("the lane has no blocked command")?;
        if rule == 0 || !rules.contains(&rule) {
            return Err(format!(
                "rule {rule} did not block the latest command (rules {rules:?}; rule 0 is never granted)"
            ));
        }
        rec.grants.push(record::Grant {
            rule,
            digest,
            reason: reason.to_string(),
            used: false,
        });
        self.save(&rec, Some(rev), None).await?;
        Ok(format!(
            "one attempt of the latest blocked command granted for rule {rule}; the lane must retry it"
        ))
    }

    /// Validation, preview, dedupe, then the start (LANE req. 1-16).
    async fn open(self: &Arc<Self>, input: &Value) -> Result<Value, String> {
        let mut input_value = input.clone();
        if let Some(map) = input_value.as_object_mut() {
            map.remove("op");
        }
        let thread_from = input_value
            .as_object_mut()
            .and_then(|m| m.remove("thread_from"))
            .and_then(|v| v.as_str().map(str::to_string));
        let open: OpenInput =
            serde_json::from_value(input_value.clone()).map_err(|e| format!("open: {e}"))?;
        let config = self.cfg();
        let valid = record::validate(&open, &config.lanes)?;
        let preview_rec = Self::build(&valid, &config, String::new());
        if open.preview {
            let (rules, _) = self.rules_for(&preview_rec).await;
            return Ok(json!({
                "preview": true,
                "brief": record::kickoff(&preview_rec, &rules, &[]),
                "warnings": valid.warnings,
            }));
        }
        if !open.after.is_empty() {
            if thread_from.is_some() {
                return Err("a queued launch takes thread, not thread_from".into());
            }
            return self.queue(&open, &valid, input_value).await;
        }
        if docs::get::<queue::Queued>(&self.env.store, docs::QUEUED, &valid.key)
            .await?
            .is_some_and(|(q, _)| q.reserves())
        {
            return Err(format!(
                "the key {} is reserved by a queued launch; start it with op start",
                valid.key
            ));
        }
        let digest = record::digest(&open);
        if let Some(rid) = &open.request_id {
            let id = rid.clone();
            let earlier = self
                .env
                .store
                .read(move |r| r.open_dedupe(&id))
                .await
                .map_err(|e| e.to_string())?;
            if let Some((d, key, outcome)) = earlier {
                if d != digest {
                    return Err("this request ID was used for an open with other inputs".into());
                }
                return match outcome {
                    Some(o) => Ok(json!({ "key": key, "outcome": o, "repeat": true })),
                    None => Err(
                        "an earlier open with this request ID is unconfirmed; inspect first".into(),
                    ),
                };
            }
        }
        {
            let mut opening = self.opening.lock().unwrap_or_else(PoisonError::into_inner);
            if opening.contains(&valid.key) {
                return Err(format!("busy: lane {} is being opened", valid.key));
            }
            opening.push(valid.key.clone());
        }
        let result = self
            .open_one(valid, open.request_id.clone(), digest, thread_from)
            .await;
        self.opening
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|k| k != &open.key);
        let outcome = result?;
        Ok(json!({ "key": open.key, "outcome": outcome }))
    }

    fn build(v: &record::Validated, config: &Config, run_id: String) -> Record {
        let allow = config
            .lanes
            .allow
            .iter()
            .filter(|a| shell::glob(&a.lanes, &v.key))
            .flat_map(|a| a.patterns.iter().map(|p| p.as_str().to_string()))
            .collect();
        let now = now_ms();
        Record {
            key: v.key.clone(),
            title: v.title.clone(),
            brief: v.brief.clone(),
            checks: v.checks.clone(),
            thread: v.thread.clone(),
            cwd: v.cwd.display().to_string(),
            model: v.model.clone(),
            thinking: v.thinking.clone(),
            origin: v.origin.clone(),
            look_only: v.look_only,
            ceilings: v.ceilings,
            worktrees: Vec::new(),
            state: State::Starting,
            prev: None,
            outcome: None,
            opened_ms: now,
            closed_ms: None,
            run_id,
            handle: None,
            session_id: None,
            session_file: None,
            earlier_ids: Vec::new(),
            pid: None,
            start_note: None,
            held_by: None,
            hold_reason: None,
            claim: None,
            objection: Vec::new(),
            intent: None,
            receipt: Vec::new(),
            lost: None,
            deny: config
                .lanes
                .deny
                .iter()
                .map(|r| r.as_str().to_string())
                .collect(),
            allow,
            grants: Vec::new(),
            last_block: None,
            doubt: None,
            watch: record::Watch {
                round_ms: now,
                ..record::Watch::default()
            },
            spend: None,
            priority: v.priority,
            runtime: config.sessions.runtime,
        }
    }

    async fn open_one(
        self: &Arc<Self>,
        valid: record::Validated,
        request_id: Option<String>,
        digest: String,
        thread_from: Option<String>,
    ) -> Result<String, String> {
        if self.row(&valid.key).await?.is_some() {
            return Err(format!("the key {} is already used by a lane", valid.key));
        }
        let config = self.cfg();
        let exe = self.admission(&config, &valid.cwd).await?;
        let mut rec = Self::build(&valid, &config, crate::host::random_hex(8));
        self.tern_spend(&config, &rec).await?;
        if let Some(source) = thread_from {
            rec.thread = Some(self.thread_for(&rec.key, &rec.title, &source).await?);
        }
        if let Some(w) = &valid.worktree {
            git::add_worktree(&valid.cwd, &w.path, &w.branch, &w.base).await?;
            rec.worktrees.push(w.path.clone());
        }
        // Record first, then spawn (LANE req. 15c).
        let rev = match self.save(&rec, None, Some("opened".into())).await {
            Ok(rev) => rev,
            Err(e) => {
                for w in &rec.worktrees {
                    let _ = git::remove_worktree(w).await;
                }
                return Err(e);
            }
        };
        if let Some(rid) = &request_id {
            let _ = self
                .env
                .store
                .lanes(LaneWrite::Dedupe {
                    request_id: rid.clone(),
                    digest,
                    key: rec.key.clone(),
                })
                .await;
        }
        let outcome = self.start(rec, rev, exe, &config, None).await;
        if let Some(rid) = request_id
            && outcome != "unconfirmed"
        {
            let _ = self
                .env
                .store
                .lanes(LaneWrite::DedupeOutcome {
                    request_id: rid,
                    outcome: outcome.clone(),
                })
                .await;
        }
        if outcome.starts_with("failed") {
            return Err(outcome);
        }
        Ok(outcome)
    }

    /// A thread for the lane from the owner's message, journaled before the request (DISC-OUT
    /// req. 53-57).
    async fn thread_for(&self, key: &str, title: &str, source: &str) -> Result<String, String> {
        let (room, message) = source
            .strip_prefix("discord:")
            .and_then(|r| r.split_once('/'))
            .filter(|(r, m)| record::origin_ok(&format!("discord:{r}/{m}")))
            .ok_or("thread_from must be discord:<room>/<message>")?;
        let fingerprint = crate::config::sha256_hex(format!("create:{room}:{title}").as_bytes());
        let src = source.to_string();
        let earlier = self
            .env
            .store
            .read(move |r| r.journal(&src))
            .await
            .map_err(|e| e.to_string())?;
        let k = key.to_string();
        if let Some(other) = self
            .env
            .store
            .read(move |r| r.journal_of_lane(&k))
            .await
            .map_err(|e| e.to_string())?
            && other != source
        {
            return Err("this lane already has a thread creation".into());
        }
        if let Some(j) = earlier {
            return match (j.state.as_str(), j.thread) {
                ("confirmed", Some(t)) if j.operation == fingerprint && j.lane == key => Ok(format!("discord:{t}")),
                _ => Err("a thread creation for this message is pending or differs; nothing was recreated".into()),
            };
        }
        let journal = |state: &str, thread: Option<String>| LaneWrite::Journal {
            source: source.to_string(),
            lane: key.to_string(),
            parent: room.to_string(),
            operation: fingerprint.clone(),
            state: state.to_string(),
            thread,
        };
        self.env
            .store
            .lanes(journal("pending", None))
            .await
            .map_err(|e| e.to_string())?;
        let title = format!("[working] {title}");
        let cut: String = title.chars().take(100).collect();
        match self.env.threads.create(room, message, &cut).await {
            Ok(thread) => {
                self.env
                    .store
                    .lanes(journal("confirmed", Some(thread.clone())))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(format!("discord:{thread}"))
            }
            Err((false, why)) => {
                let _ = self
                    .env
                    .store
                    .lanes(LaneWrite::JournalDrop {
                        source: source.to_string(),
                    })
                    .await;
                Err(format!("thread not created: {why}"))
            }
            Err((true, why)) => Err(format!("thread creation uncertain, left pending: {why}")),
        }
    }

    /// The session directory, made private, and the start spec for a fresh or resumed session.
    fn start_spec(
        &self,
        rec: &Record,
        handle: String,
        resume: Option<&(String, PathBuf)>,
    ) -> Result<StartSpec, crate::sys::fs::FsError> {
        let sessions = self.env.state_dir.join("sessions");
        let dir = sessions.join("lanes").join(&rec.key);
        crate::sys::fs::ensure_private_dir(&sessions)?;
        crate::sys::fs::ensure_private_dir(&sessions.join("lanes"))?;
        crate::sys::fs::ensure_private_dir(&dir)?;
        let (target, expected) = match resume {
            Some((id, file)) => (Target::Resume(file.clone()), Some(id.clone())),
            None => (Target::Fresh(dir), None),
        };
        Ok(StartSpec {
            handle,
            role: Role::Lane,
            cwd: PathBuf::from(&rec.cwd),
            model: rec.model.clone(),
            thinking: rec.thinking.clone(),
            target,
            expected_id: expected,
            instructions: None,
            max_cost: rec.ceilings.cost,
            tools: rec.look_only.then(|| LOOK_ONLY.to_string()),
        })
    }

    /// The kickoff with the learned rules; the lead hears which eligible rules were left out
    /// (EVR req. 47).
    async fn kickoff_text(&self, rec: &Record) -> String {
        let (rules, left) = self.rules_for(rec).await;
        if !left.is_empty() {
            self.alert(
                &rec.key,
                "learned rules left out",
                &format!("over the limit at launch: {}", left.join(", ")),
                false,
            );
        }
        record::kickoff(rec, &rules, &[])
    }

    /// The start sequence after the record exists (LANE req. 15-16; HOST-19). Returns the outcome
    /// line: `started`, `start held: model`, `unconfirmed: ...` or `failed: ...`.
    async fn start(
        self: &Arc<Self>,
        mut rec: Record,
        mut rev: Revision,
        exe: PathBuf,
        config: &Config,
        resume: Option<(String, PathBuf)>,
    ) -> String {
        let began = Instant::now();
        let handle = crate::host::random_hex(8);
        rec.handle = Some(handle.clone());
        let spec = match self.start_spec(&rec, handle, resume.as_ref()) {
            Ok(spec) => spec,
            Err(e) => return self.start_failed(rec, rev, None, &e.to_string()).await,
        };
        // The bridge says hello while OMP starts; its handle must already name this lane.
        match self.save(&rec, Some(rev), None).await {
            Ok(r) => rev = r,
            Err(e) => return self.start_failed(rec, rev, None, &e).await,
        }
        let runtime = (self.env.make)(config, &exe, rec.runtime);
        let budget = config.lanes.start_budget;
        let started = tokio::time::timeout(budget, runtime.start(spec)).await;
        let (session, started) = match started {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return self.start_failed(rec, rev, None, &e.to_string()).await,
            Err(_) => {
                return self
                    .start_failed(rec, rev, None, "the start deadline passed")
                    .await;
            }
        };
        self.live_map()
            .insert(rec.key.clone(), Arc::clone(&session));
        rec.session_id = Some(started.session_id.clone());
        rec.session_file = Some(started.file.clone());
        rec.pid = started.pid;
        tokio::spawn(Arc::clone(self).watch(rec.key.clone(), Arc::clone(&session)));
        if let Some(reason) = started.held {
            // Never substitute a model; keep the process for inspection (LANE req. 15f).
            rec.state = State::Held;
            rec.held_by = Some("hub".into());
            rec.hold_reason = Some(reason.clone());
            rec.start_note = Some("start held: model".into());
            let _ = self
                .save(&rec, Some(rev), Some(format!("start held: {reason}")))
                .await;
            self.alert(&rec.key, "start held: model", &reason, true);
            return "start held: model".into();
        }
        match self.save(&rec, Some(rev), None).await {
            Ok(r) => rev = r,
            Err(e) => return self.start_failed(rec, rev, Some(&session), &e).await,
        }
        if resume.is_some() {
            return "started".into();
        }
        // OMP's TUI (tern) takes a goal only between turns and starts no turn for it, so a tern
        // lane's goal comes before the kickoff; over rpc (headless) a goal set while idle starts a
        // turn of its own, so it follows the kickoff, whose turn is then running.
        let goal_first = rec.runtime == crate::config::session::RuntimeKind::Tern;
        if goal_first && let Err(why) = Self::set_goal(&rec, &session).await {
            return self.unconfirmed(rec, rev, &why).await;
        }
        let deadline = began + budget;
        let kickoff = self.kickoff_text(&rec).await;
        let delivered = self
            .deliver_wait(
                &rec.key,
                &session,
                &kickoff,
                &format!("kickoff:{}", rec.run_id),
                deadline,
            )
            .await;
        if delivered != ReceiptState::Delivered {
            return self
                .unconfirmed(
                    rec,
                    rev,
                    "the kickoff was not confirmed in the session file",
                )
                .await;
        }
        self.start_latency.record(began.elapsed());
        if !goal_first && let Err(why) = Self::set_goal(&rec, &session).await {
            return self.unconfirmed(rec, rev, &why).await;
        }
        rec.state = State::Running;
        let note = "started".to_string();
        if let Err(e) = self.save(&rec, Some(rev), Some(note)).await {
            return self.unconfirmed(rec, rev, &e).await;
        }
        self.status_word(&rec).await;
        self.emit(
            Emit::new(EventKind::LaneStarted, Severity::Info, false, &rec.key)
                .with("lane", &rec.key)
                .with("detail", format!("started on {} in {}", rec.model, rec.cwd)),
        );
        self.nudge.notify_one();
        "started".into()
    }

    /// Nothing reached the agent: stop the process, remove a created worktree, delete the record
    /// (LANE req. 16).
    async fn start_failed(
        &self,
        rec: Record,
        rev: Revision,
        session: Option<&Arc<R::Session>>,
        why: &str,
    ) -> String {
        if let Some(s) = session {
            let _ = s.close(true).await;
        }
        self.live_map().remove(&rec.key);
        for w in &rec.worktrees {
            let _ = git::remove_worktree(w).await;
        }
        if let Err(e) = self
            .env
            .store
            .lanes(LaneWrite::Delete {
                key: rec.key.clone(),
                expect: rev,
            })
            .await
        {
            log::warn(
                "failed lane record not deleted",
                &[("lane", &rec.key), ("error", &e)],
            );
        }
        format!("failed: {why}")
    }

    /// The kickoff may have reached the agent: keep record and process, mark it for inspection.
    async fn unconfirmed(&self, mut rec: Record, rev: Revision, why: &str) -> String {
        rec.state = State::Held;
        rec.held_by = Some("hub".into());
        rec.hold_reason = Some(why.to_string());
        rec.start_note = Some("unconfirmed".into());
        let _ = self
            .save(&rec, Some(rev), Some(format!("start unconfirmed: {why}")))
            .await;
        self.alert(&rec.key, "start unconfirmed", why, true);
        format!("unconfirmed: {why}")
    }

    /// Sets the lane's goal (HOST-19) and checks it reads back set in the same session; `Err` is
    /// why not.
    async fn set_goal(rec: &Record, session: &Arc<R::Session>) -> Result<(), String> {
        let goal = session.goal("create", Some(&record::objective(rec))).await;
        let goal_ok = matches!(
            goal.as_ref().map(|g| g.status),
            Ok(crate::host::dialogs::GoalStatus::Active
                | crate::host::dialogs::GoalStatus::Complete)
        );
        let same = session.alive()
            && session.snapshot().session_id.as_deref() == rec.session_id.as_deref();
        match goal {
            _ if goal_ok && same => Ok(()),
            Err(e) => Err(format!("goal not set: {e}")),
            Ok(g) if !goal_ok => Err(format!("goal reads back {:?}", g.status)),
            Ok(_) => Err("the session changed during the start".into()),
        }
    }

    /// Stores a message in the lane's inbox and waits until its entry is in the session file, it is
    /// in doubt or failed, or the deadline passes.
    async fn deliver_wait(
        &self,
        key: &str,
        session: &Arc<R::Session>,
        text: &str,
        identity: &str,
        deadline: Instant,
    ) -> ReceiptState {
        let mailbox = self.mailbox(key);
        let message = Message {
            sender: HUB_SENDER.into(),
            text: text.to_string(),
            urgent: true,
            owner: false,
            identity: Some(identity.to_string()),
            request_id: None,
        };
        let Ok((receipt, id, _)) = mailbox.deliver(&message, Some(&**session), false).await else {
            return ReceiptState::Failed;
        };
        let Some(id) = id else {
            return receipt.state;
        };
        loop {
            let state = mailbox.receipt(None, id).await.state;
            if !matches!(state, ReceiptState::Waiting | ReceiptState::Sent)
                || Instant::now() >= deadline
            {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = mailbox.flush(Some(&**session), false).await;
        }
    }

    /// Puts a note or wake in the lane's inbox and flushes it now when the lane can read.
    async fn tell(&self, key: &str, text: String, identity: Option<String>, urgent: bool) {
        let mailbox = self.mailbox(key);
        let session = self.session(key);
        let message = Message {
            sender: "lead".into(),
            text,
            urgent,
            owner: false,
            identity,
            request_id: None,
        };
        if let Err(e) = mailbox.deliver(&message, session.as_deref(), false).await {
            log::warn("lane notice not stored", &[("lane", &key), ("error", &e)]);
        }
    }

    /// Lead-to-lane message (EVR req. 59-63, LANE req. 33).
    async fn send(
        &self,
        key: &str,
        text: &str,
        revision: Option<&str>,
        id: Option<&str>,
        steer: bool,
    ) -> Result<String, String> {
        let turn = self.turn(key).await?;
        let (rec, rev) = self.load(key).await?;
        if !rec.open() || rec.intent.is_some() {
            return Err("the lane is closed or closing".into());
        }
        if rec.state == State::Held {
            return Err("the lane is held; resume it first".into());
        }
        if let Some(d) = &rec.doubt {
            return Err(format!(
                "an earlier delivery to this lane is in doubt ({d}); release it first"
            ));
        }
        if revision.is_some_and(|r| r != revision_text(rev)) {
            return Err(crate::store::lanes::MOVED_ON.into());
        }
        let session = self
            .session(key)
            .ok_or("the lane has no confirmed live session")?;
        let mailbox = self.mailbox(key);
        if let Some(id) = id
            && let Ok(Some(found)) = {
                let (t, i) = (key.to_string(), id.to_string());
                self.env.store.read(move |r| r.receipt(&t, &i)).await
            }
        {
            let state = match found.inbox_id {
                Some(row) => mailbox.receipt(Some(id.to_string()), row).await.state,
                None => ReceiptState::Duplicate,
            };
            return Ok(format!("message {id} earlier: {state:?}"));
        }
        let message = Message {
            sender: "lead".into(),
            text: text.to_string(),
            urgent: true,
            owner: false,
            identity: None,
            request_id: id.map(str::to_string),
        };
        if steer {
            let snap = session.snapshot();
            if snap.status != Status::Working {
                return Err("steering needs a running turn".into());
            }
            let out = session
                .deliver(crate::host::Delivery {
                    expected_session: rec.session_id.clone().unwrap_or_default(),
                    text: text.to_string(),
                    timing: crate::host::Timing::Working,
                    steer: true,
                    fresh: false,
                })
                .await;
            return Ok(format!("{out:?}"));
        }
        let (receipt, _, _) = mailbox.deliver(&message, Some(&*session), false).await?;
        if receipt.state == ReceiptState::InDoubt {
            drop(turn);
            let reason = receipt.reason.clone().unwrap_or_default();
            self.keep(key, |r| r.doubt = Some(reason)).await;
        }
        self.nudge.notify_one();
        Ok(format!("{:?}", receipt.state).to_lowercase())
    }

    /// Lifts the in-doubt fence; never resends, never decides what happened (EVR req. 62).
    async fn release(&self, key: &str, note: &str) -> Result<String, String> {
        let _turn = self.turn(key).await?;
        let (mut rec, rev) = self.load(key).await?;
        let was = rec
            .doubt
            .take()
            .ok_or("no delivery to this lane is in doubt")?;
        self.save(
            &rec,
            Some(rev),
            Some(format!("in-doubt delivery released: {note} ({was})")),
        )
        .await?;
        Ok("released; nothing was resent".into())
    }

    /// The last transcript entries of the lane's session (LANE req. 33).
    fn read(&self, key: &str, count: Option<u64>) -> Result<Value, String> {
        let session = self.session(key).ok_or("the lane has no live session")?;
        let n = usize::try_from(count.unwrap_or(READ_DEFAULT as u64))
            .unwrap_or(READ_DEFAULT)
            .min(READ_MAX);
        let snap = session.snapshot();
        let recent: Vec<_> = snap.recent.iter().rev().take(n).rev().cloned().collect();
        Ok(
            json!({ "status": snap.status, "goal": snap.goal, "dialogs": snap.dialogs, "entries": recent }),
        )
    }

    /// A control that names the revision it saw (LANE req. 33).
    async fn control(
        &self,
        key: &str,
        revision: Option<&str>,
        f: impl FnOnce(Arc<R::Session>) -> BoxFut<'static, Result<String, String>>,
    ) -> Result<String, String> {
        let _turn = self.turn(key).await?;
        let (rec, rev) = self.load(key).await?;
        if revision.is_some_and(|r| r != revision_text(rev)) || !rec.open() {
            return Err(crate::store::lanes::MOVED_ON.into());
        }
        let session = self.session(key).ok_or("the lane has no live session")?;
        if session.snapshot().session_id != rec.session_id {
            return Err(crate::store::lanes::MOVED_ON.into());
        }
        f(session).await
    }

    /// A lead-approved model change: the pin moves with it (EXT req. 39).
    async fn set_model(
        &self,
        key: &str,
        revision: Option<&str>,
        model: &str,
    ) -> Result<String, String> {
        if let Some(e) = record::model_error(model) {
            return Err(e);
        }
        let m = model.to_string();
        let done = self
            .control(key, revision, move |s| {
                Box::pin(async move { s.set_model(&m).await })
            })
            .await?;
        let base = model.split(':').next().unwrap_or(model).to_string();
        self.keep(key, |r| r.model = base).await;
        Ok(done)
    }

    /// Answers the lane's pending dialog (LANE req. 34).
    async fn answer(&self, key: &str, dialog: &str, answer: &Value) -> Result<(), String> {
        if answer.to_string().len() > ANSWER_MAX {
            return Err("answers over 64 KiB are refused".into());
        }
        let session = self.session(key).ok_or("the lane has no live session")?;
        let (rec, _) = self.load(key).await?;
        if !rec.open() || rec.intent.is_some() || session.snapshot().session_id != rec.session_id {
            return Err("the lane is closed, closing or bound to another session".into());
        }
        let id = rec.session_id.unwrap_or_default();
        session.answer(&id, dialog, answer).await
    }

    /// Listing (LANE req. 32): key, title, thread, state, outcome, times; held ones with reasons.
    async fn list(&self, all: bool) -> Result<Value, String> {
        let rows = self
            .env
            .store
            .read(move |r| r.lanes(!all))
            .await
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let Ok(rec) = serde_json::from_str::<Record>(&row.record) else {
                out.push(json!({ "key": row.key, "state": "damaged" }));
                continue;
            };
            out.push(json!({
                "key": rec.key,
                "title": rec.title,
                "thread": rec.thread,
                "state": rec.state.name(),
                "status": rec.status(),
                "outcome": rec.outcome,
                "opened_ms": rec.opened_ms,
                "closed_ms": rec.closed_ms,
                "revision": revision_text((row.opened_ms, row.revision)),
                "run_id": rec.run_id,
                "session_id": rec.session_id,
                "session_file": rec.session_file,
                "pid": rec.pid,
                "held_by": rec.held_by,
                "reason": rec.hold_reason.as_ref().or(rec.intent.as_ref().map(|i| &i.note)),
                "resume_condition": rec.intent.as_ref().and_then(|i| i.condition.clone()),
                "continue_with": match (rec.state, &rec.intent) {
                    (State::Held, None) => Some("resume"),
                    (State::Closed, _) | (State::Held, Some(_)) => Some("reopen"),
                    _ => None,
                },
                "lost": rec.lost,
                "claim": rec.claim,
                "objection": rec.objection,
                "receipt": rec.receipt,
                "doubt": rec.doubt,
                "live": self.session(&rec.key).is_some(),
            }));
        }
        Ok(Value::Array(out))
    }

    /// The status view (EVR-29, COORD-45): one line per open lane, or one named lane.
    async fn board(&self, key: Option<&str>) -> Result<String, String> {
        let rows = match key {
            Some(k) => self.row(k).await?.into_iter().collect(),
            None => self
                .env
                .store
                .read(|r| r.lanes(true))
                .await
                .map_err(|e| e.to_string())?,
        };
        let now = now_ms();
        let mut text = format!("{} lane(s)\n", rows.len());
        for row in rows {
            let Ok(rec) = serde_json::from_str::<Record>(&row.record) else {
                let _ = writeln!(text, "{}  unknown (record damaged)", row.key);
                continue;
            };
            let snap = self.session(&rec.key).map(|s| s.snapshot());
            let agent = snap.as_ref().map_or("gone".to_string(), |s| {
                format!("{:?}", s.status).to_lowercase()
            });
            let goal = snap
                .as_ref()
                .and_then(|s| s.goal.as_ref())
                .map_or("unknown".into(), |g| {
                    format!("{:?}", g.status).to_lowercase()
                });
            let approvals = snap.as_ref().map_or(0, |s| s.dialogs.len());
            let (k, run) = (rec.key.clone(), rec.run_id.clone());
            let last = self
                .env
                .store
                .read(move |r| r.reports(&k, Some(&run), 1))
                .await
                .ok()
                .and_then(|v| v.into_iter().next());
            let report = last.map_or("no report".into(), |r| {
                format!(
                    "{} {}: {}",
                    crate::jobs::instant_text(r.at_ms),
                    r.kind,
                    evidence::cut(&evidence::display(&r.text), 120)
                )
            });
            let _ = writeln!(
                text,
                "{} [{}] {} | {}m | agent {agent} | goal {goal} | approvals {approvals} | {}",
                rec.key,
                rec.thread.as_deref().unwrap_or("-"),
                rec.status(),
                (now - rec.opened_ms) / 60_000,
                report
            );
        }
        Ok(text)
    }

    /// The drift report (LANE req. 71); changes nothing.
    async fn drift(&self) -> Result<Value, String> {
        let rows = self
            .env
            .store
            .read(|r| r.lanes(true))
            .await
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let Ok(rec) = serde_json::from_str::<Record>(&row.record) else {
                continue;
            };
            let live = self
                .session(&rec.key)
                .map(|s| s.snapshot().session_id.clone());
            let issue = match live {
                None => Some("session gone".to_string()),
                Some(id) if id != rec.session_id => {
                    Some(format!("live session {id:?} differs from the record"))
                }
                Some(_) => None,
            };
            let empty = rec
                .checks
                .is_empty()
                .then_some("empty completion condition");
            if issue.is_some() || empty.is_some() {
                out.push(json!({ "key": rec.key, "issue": issue, "checks": empty }));
            }
        }
        Ok(Value::Array(out))
    }

    /// Owner messages in a thread go to the one open lane that names it (DISC-IN req. 41-42).
    /// `None`: no lane owns it; `Some(Err)`: held (two lanes claim it).
    pub(crate) async fn route(&self, relay: &str, room: &str) -> Option<Result<String, String>> {
        let thread = format!("{relay}:{room}");
        let rows = self.env.store.read(|r| r.lanes(true)).await.ok()?;
        let owners: Vec<String> = rows
            .into_iter()
            .filter(|r| r.thread.as_deref() == Some(thread.as_str()))
            .map(|r| r.key)
            .collect();
        match owners.as_slice() {
            [] => None,
            [one] => Some(Ok(one.clone())),
            _ => Some(Err(format!(
                "lanes {} all claim this thread",
                owners.join(", ")
            ))),
        }
    }

    /// Hands an owner message from a lane's thread to that lane (DISC-IN req. 42).
    pub(crate) async fn hand_off(
        &self,
        key: &str,
        message: Message,
    ) -> Result<crate::host::inbox::Receipt, String> {
        let (rec, _) = self.load(key).await?;
        if rec.intent.is_some() || rec.state == State::Closing {
            return Err("the lane is closing".into());
        }
        let session = self.session(key);
        if session
            .as_ref()
            .is_some_and(|s| s.snapshot().session_id != rec.session_id)
        {
            return Err("the lane's session identity cannot be verified".into());
        }
        let mailbox = self.mailbox(key);
        let (receipt, _, _) = mailbox.deliver(&message, session.as_deref(), false).await?;
        Ok(receipt)
    }

    /// The receipt of a hand-off by request ID.
    pub(crate) async fn receipt(
        &self,
        key: &str,
        request_id: &str,
    ) -> Option<crate::host::inbox::Receipt> {
        let (t, i) = (key.to_string(), request_id.to_string());
        let found = self
            .env
            .store
            .read(move |r| r.receipt(&t, &i))
            .await
            .ok()??;
        let id = found.inbox_id?;
        Some(
            self.mailbox(key)
                .receipt(Some(request_id.to_string()), id)
                .await,
        )
    }

    /// The thread title word from lane state (DESIGN 12.2 C17).
    async fn status_word(&self, rec: &Record) {
        let Some(thread) = rec
            .thread
            .as_deref()
            .and_then(|t| t.strip_prefix("discord:"))
        else {
            return;
        };
        let (word, archive) = match rec.state {
            State::Running | State::Rejected | State::Starting => ("working", false),
            State::Held | State::Claimed | State::Closing => ("waiting", false),
            State::Closed => ("done", rec.outcome.as_deref() == Some("completed")),
        };
        if let Err(e) = self.env.threads.status(thread, word, archive).await {
            log::info(
                "thread status not set",
                &[("lane", &rec.key), ("error", &e)],
            );
        }
    }

    /// Spend per lane (WS-22): its session and its subagents' totals over the lane's interval.
    async fn spend_view(&self, key: Option<&str>) -> Result<Value, String> {
        let rows = match key {
            Some(k) => self.row(k).await?.into_iter().collect(),
            None => self
                .env
                .store
                .read(|r| r.lanes(false))
                .await
                .map_err(|e| e.to_string())?,
        };
        let mut out = Vec::new();
        for row in rows {
            let Ok(rec) = serde_json::from_str::<Record>(&row.record) else {
                continue;
            };
            let totals = rec.spend.map_or_else(|| self.read_spend(&rec), Ok);
            out.push(match totals {
                Ok((tokens, cost)) => json!({ "key": rec.key, "tokens": tokens, "cost_usd": cost }),
                Err(issue) => json!({ "key": rec.key, "tokens": "unknown", "cost_usd": "unknown", "issue": issue }),
            });
        }
        Ok(json!({
            "lanes": out,
            "note": "cost is OMP's per-call estimate at API list prices, not an invoice"
        }))
    }

    /// Reads the lane's session file and its subagent and advisor transcripts incrementally (WS req.
    /// 30-35): a ceiling that ignored subagents could be passed by spawning them.
    fn read_spend(&self, rec: &Record) -> Result<(f64, f64), String> {
        let file = rec.session_file.as_deref().ok_or("no session file")?;
        let mut map = self.spend.lock().unwrap_or_else(PoisonError::into_inner);
        let tree = map.entry(rec.key.clone()).or_default();
        tree.read(Path::new(file), rec.opened_ms)
            .map(|(tokens, cost, _)| (tokens, cost))
    }
}
