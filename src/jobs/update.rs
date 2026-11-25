//! The release notice (IR-21, IR-22; req. 36-39; owner decision 12): once a day the hub reads
//! GitHub's latest stable release and tells the owner, once per version, when it is newer than this
//! binary. It never downloads, installs or restarts anything; `cataly update` does that when the
//! owner runs it. A version the owner rolled back from is never announced again. With the check off
//! no request is made. The notice, and the message after the hub first runs an updated or rolled
//! back version, go to `[update] destinations`, else to the owner's home room through the normal
//! outgoing path; they are never work for the lead.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::Jobs;
use super::event::{self, Emit, EventKind, Severity, TEXT_MAX, Template, one_line};
use super::memory::Memory;
use crate::config::{Config, HOME_DESTINATION, Language};
use crate::log;
use crate::proto::{LeadStart, Resumed};
use crate::store::{Apply, NewEvent};
use crate::sys::install::Layout;
use crate::sys::now_ms;
use crate::sys::release::{self, Release, Version};

/// The memory this task keeps (survives a restart, IR req. 39).
const JOB: &str = "update";
const SUBJECT: &str = "release";
/// The memory subject holding the version of the hub's last run.
const HUB: &str = "hub";
/// One check per day; the next is due a day after the attempt, whatever its result (IR req. 36).
const EVERY_MS: i64 = 24 * 60 * 60 * 1000;
/// How often the task looks at the config and the clock while it waits.
const LOOK: Duration = Duration::from_secs(30);

/// The sessions the hub brought back at start; `None` while it is still bringing them back.
pub(crate) type ResumedView = Arc<dyn Fn() -> Option<Resumed> + Send + Sync>;

/// How long the message after an update waits for the sessions to come back.
const RESUME_WAIT: Duration = Duration::from_mins(1);

/// Runs until the hub stops. `layout` is the program folder holding the silenced version and the
/// install records. First, once per version change, the owner hears that the hub now runs a newer
/// (or, after a rollback, an older) version than its previous run, and which sessions it resumed.
pub(crate) async fn run(
    jobs: Jobs,
    layout: Option<Layout>,
    resumed: ResumedView,
    mut stop: watch::Receiver<bool>,
) -> Result<(), String> {
    let started = Instant::now();
    let mut memory = load(&jobs).await?;
    if let Some(previous) = changed(&mut memory, layout.as_ref()) {
        let deadline = Instant::now() + RESUME_WAIT;
        let sessions = loop {
            match resumed() {
                Some(r) => break Some(r),
                None if Instant::now() >= deadline => break None,
                None => {}
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(500)) => {}
                // Unsaved, so the next start says it.
                _ = stop.changed() => return Ok(()),
            }
        };
        let config = Arc::clone(&jobs.ctx().config.borrow());
        if let Some(event) = updated(&previous, sessions, &config) {
            jobs.ctx()
                .stream
                .append(vec![event])
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    save(&jobs, memory).await?;
    let silenced_file = layout.map(|l| l.silenced());
    loop {
        let config = Arc::clone(&jobs.ctx().config.borrow());
        let mut memory = load(&jobs).await?;
        let last = memory
            .get(SUBJECT, "last_check_ms")
            .and_then(|v| v.parse::<i64>().ok());
        let since_start = started.elapsed();
        let wait_start = config.update.first_after.saturating_sub(since_start);
        let wait_day = last.map_or(Duration::ZERO, |l| {
            Duration::from_millis(u64::try_from(l + EVERY_MS - now_ms()).unwrap_or(0))
        });
        let wait = if config.update.check {
            wait_start.max(wait_day)
        } else {
            LOOK
        };
        if !wait.is_zero() {
            tokio::select! {
                () = tokio::time::sleep(wait.min(LOOK)) => continue,
                _ = stop.changed() => return Ok(()),
            }
        }
        memory.transition(SUBJECT, "last_check_ms", &now_ms().to_string());
        let api = config.update.api()?;
        let fetched = tokio::select! {
            r = release::fetch_release(&api, None) => r,
            _ = stop.changed() => return Ok(()),
        };
        let notice = match fetched {
            Ok(Some(latest)) => notice(
                &mut memory,
                &latest,
                silenced(silenced_file.as_ref()).as_ref(),
                &config,
            ),
            Ok(None) => None,
            Err(e) => {
                log::warn("release check failed", &[("error", &e)]);
                None
            }
        };
        if let Some(notice) = notice {
            jobs.ctx()
                .stream
                .append(vec![notice])
                .await
                .map_err(|e| e.to_string())?;
        }
        save(&jobs, memory).await?;
    }
}

async fn load(jobs: &Jobs) -> Result<Memory, String> {
    let rows = jobs
        .ctx()
        .store
        .read(|r| r.memory(JOB, false))
        .await
        .map_err(|e| e.to_string())?;
    Ok(Memory::new(JOB, false, rows))
}

async fn save(jobs: &Jobs, memory: Memory) -> Result<(), String> {
    jobs.ctx()
        .store
        .apply(Apply {
            memory: memory.into_writes(),
            ..Apply::default()
        })
        .await
        .map(drop)
        .map_err(|e| e.to_string())
}

/// The version the owner rolled back from, if any.
fn silenced(file: Option<&PathBuf>) -> Option<Version> {
    let bytes = crate::sys::fs::read_private(file?, 64).ok()??;
    Version::parse(String::from_utf8_lossy(&bytes).trim())
}

/// The previous run's version when this run's differs: an update or a rollback, once per change,
/// never on an ordinary restart. The previous run's version is kept in this task's memory; before
/// any run recorded one, the install records' previous version counts when this binary is the
/// installed current one.
fn changed(memory: &mut Memory, layout: Option<&Layout>) -> Option<String> {
    let latest = release::VERSION;
    let recorded = memory.get(HUB, "version").map(str::to_string);
    memory.transition(HUB, "version", latest);
    let previous = recorded.or_else(|| {
        let l = layout.filter(|l| l.current_version().as_deref() == Some(latest))?;
        Some(crate::sys::install::role(l, "previous")?.version)
    })?;
    (previous != latest).then_some(previous)
}

/// The message after an update or a rollback from `previous`, with the sessions resumed.
fn updated(previous: &str, resumed: Option<Resumed>, config: &Config) -> Option<NewEvent> {
    let latest = release::VERSION;
    let (old, new) = (Version::parse(previous)?, Version::parse(latest)?);
    if old == new {
        return None;
    }
    let rolled_back = new < old;
    let language = config.hub.language;
    let template = config
        .update
        .updated(language, rolled_back)
        .map_err(|e| log::warn("update message template", &[("error", &e)]))
        .ok()?;
    let kind = if rolled_back {
        EventKind::UpdateRolledBack
    } else {
        EventKind::UpdateDone
    };
    let emit = Emit::new(kind, Severity::Info, false, "hub")
        .with("previous", previous)
        .with("latest", latest)
        .with("sessions", resumed_text(language, resumed));
    Some(post(emit, &template, config))
}

/// What the hub brought back after an update, in the owner's language: how the lead came back (a
/// fresh lead says why) and the lanes running again by name; empty when nothing is known or there is
/// nothing to say. A hub before 0.1.2 says only whether the lead runs and how many lanes do.
pub(crate) fn resumed_text(language: Language, resumed: Option<Resumed>) -> String {
    let Some(resumed) = resumed else {
        return String::new();
    };
    let korean = language == Language::Korean;
    let lead = match resumed.lead_start {
        None if resumed.lead && korean => Some("lead가 실행 중입니다.".to_string()),
        None if resumed.lead => Some("The lead is running.".to_string()),
        None => None,
        Some(how) => {
            let lost = |ko: &str, en: &str| {
                if korean {
                    format!("lead는 이전 대화를 이어가지 못해 새로 시작했습니다({ko}).")
                } else {
                    format!(
                        "The lead could not continue its earlier conversation and started fresh ({en})."
                    )
                }
            };
            Some(match (how, korean) {
                (LeadStart::Resumed, true) => "lead를 이어서 열었습니다.".into(),
                (LeadStart::Resumed, false) => "Resumed the lead.".into(),
                (LeadStart::First, true) => "lead를 새로 시작했습니다(이전 대화 없음).".into(),
                (LeadStart::First, false) => {
                    "Started the lead fresh (no earlier conversation).".into()
                }
                (LeadStart::Asked, true) => "lead를 요청대로 새로 시작했습니다.".into(),
                (LeadStart::Asked, false) => "Started the lead fresh, as asked.".into(),
                (LeadStart::Gone, _) => lost("대화 기록 파일이 없음", "its transcript is gone"),
                (LeadStart::Unverified, _) => lost(
                    "대화 기록이 재개 검사를 통과하지 못함",
                    "its transcript did not pass the resume check",
                ),
                (LeadStart::Ended, _) => lost(
                    "이전 세션이 이어갈 수 없는 상태로 끝남",
                    "the earlier session ended in a state that is not resumed",
                ),
                (LeadStart::NotRunning, true) => "lead는 실행 중이 아닙니다.".into(),
                (LeadStart::NotRunning, false) => "The lead is not running.".into(),
            })
        }
    };
    let n = resumed.lanes.max(resumed.lane_names.len());
    let names = resumed.lane_names.join(", ");
    let lanes = match (n, names.is_empty(), korean) {
        (0, _, _) => None,
        (n, true, true) => Some(format!("lane {n}개를 이어서 열었습니다.")),
        (n, false, true) => Some(format!("lane {n}개를 이어서 열었습니다: {names}.")),
        (1, true, false) => Some("Resumed 1 lane.".into()),
        (1, false, false) => Some(format!("Resumed 1 lane: {names}.")),
        (n, true, false) => Some(format!("Resumed {n} lanes.")),
        (n, false, false) => Some(format!("Resumed {n} lanes: {names}.")),
    };
    lead.into_iter().chain(lanes).collect::<Vec<_>>().join(" ")
}

/// The notice for `latest`, once per version, never for a silenced one or an older one (IR req. 37).
fn notice(
    memory: &mut Memory,
    latest: &Release,
    silenced: Option<&Version>,
    config: &Config,
) -> Option<NewEvent> {
    let installed = Version::parse(release::VERSION)?;
    let version = &latest.version;
    if !version.stable() || *version <= installed || silenced == Some(version) {
        return None;
    }
    let template = config
        .update
        .template(config.hub.language)
        .map_err(|e| log::warn("update notice template", &[("error", &e)]))
        .ok()?;
    if !memory.transition(SUBJECT, "announced", &version.to_string()) {
        return None;
    }
    let emit = Emit::new(
        EventKind::UpdateAvailable,
        Severity::Attention,
        false,
        "hub",
    )
    .with("current", installed)
    .with("latest", version)
    .with(
        "url",
        format!(
            "https://github.com/{}/releases/tag/{}",
            release::REPOSITORY,
            latest.tag
        ),
    );
    Some(post(emit, &template, config))
}

/// The event in `template`'s wording, to `[update] destinations`, else the owner's home room.
fn post(emit: Emit, template: &Template, config: &Config) -> NewEvent {
    let text = template.render(&emit.values);
    let destinations = config.update.destinations.clone().unwrap_or_else(|| {
        config
            .destination(HOME_DESTINATION)
            .map_or_else(Vec::new, |_| vec![HOME_DESTINATION.to_string()])
    });
    let mut event = event::build("hub", emit, &[], false, &destinations, now_ms());
    // The wording keeps its line breaks; each line is cleaned and bounded like any event text.
    event.text = text
        .lines()
        .map(|line| one_line(line, TEXT_MAX))
        .collect::<Vec<_>>()
        .join("\n");
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Destination};
    use crate::relay::discord::ChannelRef;

    fn release(tag: &str) -> Release {
        Release {
            tag: tag.into(),
            version: Version::parse(tag).unwrap(),
            assets: Vec::new(),
        }
    }

    const DISCORD: &str =
        "[discord]\nowner = \"100000000000000001\"\nchannels = [\"100000000000000010\"]\n";

    #[test]
    fn announces_once_and_never_a_silenced_version() {
        let config = config::parse("").unwrap();
        let mut memory = Memory::new(JOB, false, Vec::new());
        let mut n = |t: &str, s: Option<&str>| {
            let s = s.map(|s| Version::parse(s).unwrap());
            notice(&mut memory, &release(t), s.as_ref(), &config)
        };
        assert!(n("v0.0.1", None).is_none(), "older");
        assert!(n("v99.0.0-rc.1", None).is_none(), "prerelease");
        assert!(n("v99.0.0", Some("99.0.0")).is_none(), "silenced");
        let first = n("v99.0.0", None).unwrap();
        assert!(first.destinations.is_empty(), "no relay, log only");
        assert!(n("v99.0.0", None).is_none(), "once");
        assert!(n("v99.1.0", None).is_some());
    }

    #[test]
    fn newer_release_goes_once_to_the_home_room_in_korean() {
        let config = config::parse(DISCORD).unwrap();
        let mut memory = Memory::new(JOB, false, Vec::new());
        let event = notice(&mut memory, &release("v99.0.0"), None, &config).unwrap();
        assert_eq!(event.destinations, [HOME_DESTINATION]);
        assert_eq!(
            config.destination(HOME_DESTINATION),
            Some(Destination::Discord {
                channel: ChannelRef::parse("100000000000000010").unwrap()
            })
        );
        assert_eq!(
            event.text,
            format!(
                "새로운 버전이 나왔습니다! 보스. 콘솔 창에 `cataly update` 명령어를 입력해주세요.\n\
                 {} -> 99.0.0 https://github.com/{}/releases/tag/v99.0.0",
                release::VERSION,
                release::REPOSITORY
            )
        );
        assert!(!event.urgent);
        assert!(notice(&mut memory, &release("v99.0.0"), None, &config).is_none());
    }

    #[test]
    fn template_language_and_destinations_override() {
        let text = format!(
            "{DISCORD}[destinations.phone]\nkind = \"command\"\nargv = [\"true\"]\n\
             [update]\ntemplate = \"new {{latest}} (was {{current}}): {{url}}\"\n\
             destinations = [\"phone\", \"log\"]\n"
        );
        let config = config::parse(&text).unwrap();
        let mut memory = Memory::new(JOB, false, Vec::new());
        let event = notice(&mut memory, &release("v99.0.0"), None, &config).unwrap();
        assert_eq!(event.destinations, ["phone"]);
        assert!(
            event
                .text
                .starts_with(&format!("new 99.0.0 (was {}): https://", release::VERSION)),
            "{}",
            event.text
        );
        let config = config::parse("[hub]\nlanguage = \"en\"\n").unwrap();
        let event = notice(&mut memory, &release("v99.2.0"), None, &config).unwrap();
        assert!(
            event
                .text
                .starts_with("A new version is out, boss. Run `cataly update` in a terminal.\n")
        );
        for bad in [
            "[update]\ntemplate = \"{version}\"\n",
            "[hub]\nlanguage = \"fr\"\n",
            "[update]\nlanguage = \"en\"\n",
            "[update]\ndestinations = [\"nowhere\"]\n",
        ] {
            assert!(config::parse(bad).is_err(), "{bad}");
        }
    }

    /// The message after a version change: once, in the owner's language, never on a restart of
    /// the same version, naming the sessions resumed; a rollback has its own wording;
    /// `updated_template` replaces the update's.
    #[test]
    fn updated_and_rolled_back_once_per_change() {
        let (older, newer, v) = ("0.0.1", "999.0.0", release::VERSION);
        let mut memory = Memory::new(JOB, false, Vec::new());
        assert_eq!(changed(&mut memory, None), None, "first run");
        assert_eq!(changed(&mut memory, None), None, "same version");
        let mut memory = Memory::new(
            JOB,
            false,
            vec![(HUB.into(), "version".into(), older.into())],
        );
        assert_eq!(changed(&mut memory, None).as_deref(), Some(older));
        assert_eq!(changed(&mut memory, None), None, "once");

        let config = config::parse(DISCORD).unwrap();
        let both = Some(Resumed {
            lead: true,
            lanes: 2,
            lead_start: Some(LeadStart::Resumed),
            lane_names: vec!["build".into(), "docs".into()],
        });
        let event = updated(older, both.clone(), &config).unwrap();
        assert_eq!(event.kind, "update.done");
        assert_eq!(event.destinations, [HOME_DESTINATION]);
        assert_eq!(
            event.text,
            format!(
                "{v} 버전으로 업데이트했습니다. 보스, 감사합니다!\n{older} -> {v}\n\
                 lead를 이어서 열었습니다. lane 2개를 이어서 열었습니다: build, docs."
            )
        );
        // The owner's 0.1.1 update: the lead started fresh, so the message says so and why.
        let fresh = Some(Resumed {
            lead: false,
            lanes: 0,
            lead_start: Some(LeadStart::Unverified),
            lane_names: Vec::new(),
        });
        let event = updated(older, fresh, &config).unwrap();
        assert!(
            event.text.ends_with(
                "\nlead는 이전 대화를 이어가지 못해 새로 시작했습니다(대화 기록이 재개 검사를 통과하지 못함)."
            ),
            "{}",
            event.text
        );
        assert!(!event.text.contains("이어서 열었습니다"), "{}", event.text);
        let event = updated(newer, None, &config).unwrap();
        assert_eq!(event.kind, "update.rolled_back");
        assert_eq!(
            event.text,
            format!("{v} 버전으로 되돌렸습니다.\n{newer} -> {v}")
        );

        let english = config::parse(&format!("{DISCORD}[hub]\nlanguage = \"en\"\n")).unwrap();
        let lead = Some(Resumed {
            lead: true,
            lanes: 1,
            lead_start: Some(LeadStart::Resumed),
            lane_names: vec!["build".into()],
        });
        let event = updated(older, lead, &english).unwrap();
        assert_eq!(
            event.text,
            format!(
                "Updated to {v}. Thanks, boss!\n{older} -> {v}\nResumed the lead. Resumed 1 lane: build."
            )
        );
        assert_eq!(
            resumed_text(
                Language::English,
                Some(Resumed {
                    lead: false,
                    lanes: 0,
                    lead_start: Some(LeadStart::Gone),
                    lane_names: Vec::new(),
                })
            ),
            "The lead could not continue its earlier conversation and started fresh (its transcript is gone)."
        );
        // A 0.1.1 hub reports only that the lead runs: never claimed as resumed.
        let old_hub = Some(Resumed {
            lead: true,
            lanes: 2,
            lead_start: None,
            lane_names: Vec::new(),
        });
        assert_eq!(
            resumed_text(Language::Korean, old_hub),
            "lead가 실행 중입니다. lane 2개를 이어서 열었습니다."
        );
        let event = updated(newer, None, &english).unwrap();
        assert!(event.text.starts_with("Rolled back to "), "{}", event.text);

        let own = config::parse(&format!(
            "{DISCORD}[update]\nupdated_template = \"now {{latest}} (was {{previous}})\"\n"
        ))
        .unwrap();
        let event = updated(older, both, &own).unwrap();
        assert_eq!(event.text, format!("now {v} (was {older})"));
        assert!(config::parse("[update]\nupdated_template = \"{url}\"\n").is_err());
    }

    /// With the check off, no request reaches the release API.
    #[tokio::test]
    async fn switched_off_makes_no_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg = format!(
            "{DISCORD}[update]\ncheck = false\nfirst_after = \"1s\"\napi_base = \"http://127.0.0.1:{port}\"\n"
        );
        let (ctx, _stop, _dir) = crate::jobs::tests::ctx_with(&cfg, "update-off");
        let jobs = Jobs { ctx };
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(run(jobs, None, Arc::new(|| None), stop_rx));
        let accepted = tokio::time::timeout(Duration::from_millis(2500), listener.accept()).await;
        assert!(accepted.is_err(), "a request was made");
        stop_tx.send(true).unwrap();
        task.await.unwrap().unwrap();
    }
}
