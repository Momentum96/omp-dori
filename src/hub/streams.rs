//! The lead's stream jobs (WS-02, WS-04; WS req. 71-75): a long-running command whose output lines
//! that match a filter become events for the lead. Restart policy with backoff, an optional silence
//! alert, oversized lines to a private file. The hub owns them: they outlive the lead's OMP process,
//! are kept in the store and start again with the hub. The lead's tool forwards start, stop and list.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

use crate::jobs::Jobs;
use crate::jobs::event::{Emit, EventKind, Severity, one_line};
use crate::log;
use crate::store::Store;
use crate::store::lead::LeadWrite;
use crate::sys::command::{self, Exit, KILL_GRACE};

/// A line longer than this is not filtered; it goes to a private file (WS req. 72).
const LINE_MAX: usize = 64 * 1024;
const JOBS_MAX: usize = 16;
const ARGV_MAX: usize = 64;
const STDERR_CAP: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Spec {
    name: String,
    argv: Vec<String>,
    filter: String,
    restarts: u32,
    delay_s: u64,
    silence_m: Option<u64>,
    low: bool,
}

#[derive(Debug, Clone)]
struct State {
    phase: &'static str,
    restarts: u32,
    last_exit: Option<String>,
    next_restart: Option<Instant>,
    stop: watch::Sender<bool>,
}

/// Running stream jobs.
pub(crate) struct Streams {
    jobs: Jobs,
    store: Store,
    cwd: PathBuf,
    files: PathBuf,
    running: Mutex<HashMap<String, State>>,
}

fn int(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn spec_of(input: &Value) -> Result<Spec, String> {
    let name = input
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if !valid {
        return Err("name: 1 to 64 letters, digits, '-', '_' or '.'".into());
    }
    let argv: Vec<String> = input
        .get("argv")
        .and_then(Value::as_array)
        .map_or_default(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        });
    if argv.is_empty() || argv.len() > ARGV_MAX {
        return Err("argv: the command and its arguments, 1 to 64 words (no shell)".into());
    }
    let filter = input
        .get("filter")
        .and_then(Value::as_str)
        .unwrap_or(".")
        .to_string();
    regex::RegexBuilder::new(&filter)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| format!("filter: {e}"))?;
    let restarts = int(input, "restarts").unwrap_or(5);
    let delay_s = int(input, "delay").unwrap_or(10);
    let silence_m = int(input, "silence");
    if restarts > 100
        || !(1..=3600).contains(&delay_s)
        || silence_m.is_some_and(|m| !(1..=1440).contains(&m))
    {
        return Err("restarts 0 to 100, delay 1 to 3600 s, silence 1 to 1440 minutes".into());
    }
    Ok(Spec {
        name: name.to_string(),
        argv,
        filter,
        restarts: u32::try_from(restarts).unwrap_or(5),
        delay_s,
        silence_m,
        low: input.get("low").and_then(Value::as_bool).unwrap_or(false),
    })
}

impl Streams {
    pub(crate) fn new(jobs: Jobs, store: Store, cwd: PathBuf, files: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            jobs,
            store,
            cwd,
            files,
            running: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, State>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts the stream jobs kept in the store (WS-04: the hub restores its own jobs).
    pub(crate) async fn resume(self: &Arc<Self>) {
        let kept = self
            .store
            .read(crate::store::Reads::stream_jobs)
            .await
            .unwrap_or_default();
        for (name, spec) in kept {
            match serde_json::from_str::<Spec>(&spec) {
                Ok(spec) => self.spawn(spec),
                Err(e) => log::warn("stream job not restored", &[("job", &name), ("error", &e)]),
            }
        }
    }

    pub(crate) async fn start(self: &Arc<Self>, input: &Value) -> Result<String, String> {
        let spec = spec_of(input)?;
        {
            let running = self.lock();
            if running.contains_key(&spec.name) {
                return Err(format!(
                    "a stream job named {} exists; stop it first",
                    spec.name
                ));
            }
            if running.len() >= JOBS_MAX {
                return Err(format!("at most {JOBS_MAX} stream jobs"));
            }
        }
        let saved = serde_json::to_string(&spec).map_err(|e| e.to_string())?;
        self.store
            .lead(LeadWrite::StreamSave {
                name: spec.name.clone(),
                spec: saved,
            })
            .await
            .map_err(|e| e.to_string())?;
        let name = spec.name.clone();
        self.spawn(spec);
        Ok(format!(
            "stream job {name} started; matching lines arrive as [event stream.line]"
        ))
    }

    pub(crate) async fn stop(&self, name: &str) -> Result<String, String> {
        let state = self
            .lock()
            .remove(name)
            .ok_or(format!("no stream job named {name}"))?;
        let _ = state.stop.send(true);
        self.store
            .lead(LeadWrite::StreamRemove { name: name.into() })
            .await
            .map_err(|e| e.to_string())?;
        Ok(format!("stream job {name} stopped"))
    }

    pub(crate) fn list(&self) -> String {
        let running = self.lock();
        if running.is_empty() {
            return "no stream jobs".into();
        }
        let mut names: Vec<&String> = running.keys().collect();
        names.sort();
        let mut out = String::new();
        for name in names {
            let Some(s) = running.get(name) else { continue };
            let _ = write!(
                out,
                "- {name}: {}, {} restart(s), last exit {}",
                s.phase,
                s.restarts,
                s.last_exit.as_deref().unwrap_or("none")
            );
            if let Some(at) = s.next_restart {
                let _ = write!(
                    out,
                    ", next restart in {} s",
                    at.saturating_duration_since(Instant::now()).as_secs()
                );
            }
            out.push('\n');
        }
        out.trim_end().to_string()
    }

    fn spawn(self: &Arc<Self>, spec: Spec) {
        let (stop, stopped) = watch::channel(false);
        self.lock().insert(
            spec.name.clone(),
            State {
                phase: "running",
                restarts: 0,
                last_exit: None,
                next_restart: None,
                stop,
            },
        );
        let me = Arc::clone(self);
        tokio::spawn(async move { me.run(spec, stopped).await });
    }

    fn update(&self, name: &str, f: impl FnOnce(&mut State)) {
        if let Some(s) = self.lock().get_mut(name) {
            f(s);
        }
    }

    async fn emit(&self, kind: EventKind, spec: &Spec, urgent: bool, value: &str) {
        let field = if kind == EventKind::StreamLine {
            "line"
        } else {
            "reason"
        };
        let emit = Emit::new(kind, Severity::Info, urgent, &spec.name)
            .with("job", &spec.name)
            .with(field, value);
        if let Err(e) = self.jobs.hub_event(emit).await {
            log::warn(
                "stream event not recorded",
                &[("job", &spec.name), ("error", &e)],
            );
        }
    }

    /// Runs the command until it is stopped or its restarts are used up (WS req. 73).
    async fn run(self: Arc<Self>, spec: Spec, mut stopped: watch::Receiver<bool>) {
        let Ok(filter) = regex::RegexBuilder::new(&spec.filter)
            .size_limit(1 << 20)
            .build()
        else {
            return;
        };
        let stderr = self
            .files
            .join("streams")
            .join(format!("{}.err", spec.name));
        let _ = crate::sys::fs::ensure_private_dir(&self.files.join("streams"));
        let mut attempt: u32 = 0;
        loop {
            let ended = match command::attach(
                &spec.argv,
                Some(&self.cwd),
                &[],
                Some(&stderr),
                STDERR_CAP,
            ) {
                Ok(child) => self.watch_output(&spec, &filter, child, &mut stopped).await,
                Err(e) => Some(format!("not started: {e}")),
            };
            let Some(exit) = ended else {
                return;
            };
            self.update(&spec.name, |s| s.last_exit = Some(exit.clone()));
            if attempt >= spec.restarts {
                self.update(&spec.name, |s| s.phase = "exhausted");
                let reason = format!("exited ({exit}) and no restart is left");
                self.emit(EventKind::StreamEnded, &spec, !spec.low, &reason)
                    .await;
                return;
            }
            let wait =
                Duration::from_secs(spec.delay_s.saturating_mul(1 << attempt.min(10)).min(3600));
            attempt += 1;
            self.update(&spec.name, |s| {
                s.restarts = attempt;
                s.phase = "waiting";
                s.next_restart = Some(Instant::now() + wait);
            });
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                _ = stopped.changed() => return,
            }
            self.update(&spec.name, |s| {
                s.phase = "running";
                s.next_restart = None;
            });
        }
    }

    /// Reads one child's output; `None` when the job was stopped, else how the child ended.
    async fn watch_output(
        &self,
        spec: &Spec,
        filter: &regex::Regex,
        mut child: command::Attached,
        stopped: &mut watch::Receiver<bool>,
    ) -> Option<String> {
        let stdout = child.take_stdout()?;
        let mut lines = BufReader::new(stdout).lines();
        let silence = spec.silence_m.map(|m| Duration::from_secs(m * 60));
        let mut quiet_since = Instant::now();
        let mut alerted = false;
        loop {
            let wait = silence.map_or(Duration::from_secs(3600), |s| {
                s.saturating_sub(quiet_since.elapsed())
                    .max(Duration::from_secs(1))
            });
            let line = tokio::select! {
                line = lines.next_line() => line,
                () = tokio::time::sleep(wait) => {
                    if let Some(s) = silence && !alerted && quiet_since.elapsed() >= s {
                        alerted = true;
                        let text = format!("no output for {} minute(s); still running", s.as_secs() / 60);
                        self.emit(EventKind::StreamLine, spec, !spec.low, &text).await;
                    }
                    continue;
                }
                _ = stopped.changed() => {
                    child.close(Duration::ZERO, KILL_GRACE).await;
                    return None;
                }
            };
            let Ok(Some(line)) = line else { break };
            quiet_since = Instant::now();
            alerted = false;
            if line.len() > LINE_MAX {
                let path = self.files.join("streams").join(format!(
                    "{}-{}.txt",
                    spec.name,
                    crate::sys::now_ms()
                ));
                let note = match crate::sys::fs::write_atomic(&path, line.as_bytes()) {
                    Ok(()) => format!(
                        "a line of {} bytes, saved in {}",
                        line.len(),
                        path.display()
                    ),
                    Err(e) => format!(
                        "a line of {} bytes that could not be saved: {e}",
                        line.len()
                    ),
                };
                self.emit(EventKind::StreamLine, spec, !spec.low, &note)
                    .await;
            } else if filter.is_match(&line) {
                self.emit(
                    EventKind::StreamLine,
                    spec,
                    !spec.low,
                    &one_line(&line, 300),
                )
                .await;
            }
        }
        let (exit, _) = child.close(Duration::from_secs(5), KILL_GRACE).await;
        Some(match exit {
            Exit::Code(c) => format!("status {c}"),
            Exit::Signal(s) => format!("signal {s}"),
            Exit::Unknown => "unknown status".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn specs_are_checked() {
        assert!(spec_of(&json!({"name": "x", "argv": ["tail"]})).is_ok());
        assert!(spec_of(&json!({"name": "bad name", "argv": ["tail"]})).is_err());
        assert!(spec_of(&json!({"name": "x", "argv": []})).is_err());
        assert!(spec_of(&json!({"name": "x", "argv": ["a"], "filter": "("})).is_err());
        assert!(spec_of(&json!({"name": "x", "argv": ["a"], "restarts": 101})).is_err());
    }
}
