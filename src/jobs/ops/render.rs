//! The words and Discord embeds of the ops room, in the owner's language: the panel with its
//! buttons, the alerts and their resolved form, the usage table, and the short lines of the recent
//! list. Everything here is pure: the facts come in, a message body goes out, within Discord's
//! embed limits.

use std::fmt::Write as _;

use serde_json::{Value, json};

use crate::config::Language;
use crate::jobs::event::field;

pub(crate) const BANNER: &str = "panel-banner.jpg";
pub(crate) const THUMB: &str = "panel-thumb.jpg";
pub(crate) static BANNER_BYTES: &[u8] = include_bytes!("../../../assets/panel-banner.jpg");
pub(crate) static THUMB_BYTES: &[u8] = include_bytes!("../../../assets/panel-thumb.jpg");

/// Button IDs on the panel.
pub(crate) const REFRESH: &str = "ops:refresh";
pub(crate) const USAGE: &str = "ops:usage";
pub(crate) const MUTE: &str = "ops:mute";
pub(crate) const WATCH: &str = "ops:watch";
/// The channel select the watch button opens.
pub(crate) const WATCHED: &str = "ops:watched";
/// Discord's most values in one select; the config allows more watched channels.
pub(crate) const PICKER_MAX: usize = 25;

/// Discord's embed limits (characters).
const TITLE_MAX: usize = 256;
const DESCRIPTION_MAX: usize = 4096;
const FIELD_NAME_MAX: usize = 256;
const FIELD_VALUE_MAX: usize = 1024;
const FOOTER_MAX: usize = 2048;
const EMBED_TOTAL_MAX: usize = 6000;
/// Provider lines on the panel; the usage button shows them all.
const PROVIDERS_SHOWN: usize = 6;
const ISSUES_SHOWN: usize = 3;
const BAR_CELLS: usize = 8;

const GREEN: u32 = 0x002E_CC71;
const AMBER: u32 = 0x00F1_C40F;
/// A limit of a provider that is not held at this per cent or more makes the panel worth a look.
const HIGH_LIMIT: f64 = 90.0;
const RED: u32 = 0x00E7_4C3C;
const GREY: u32 = 0x0095_A5A6;

/// How serious the worst thing on the panel is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Level {
    Fine,
    Watch,
    Act,
}

/// The host as the pressure rules see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostState {
    /// Below the ready line: heavy compute is welcome.
    Ready,
    Busy,
    /// Over the alert line or failing a check.
    Alert,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Host {
    pub(crate) load: Option<f64>,
    pub(crate) memory_free: Option<f64>,
    pub(crate) disk_free_gb: Option<f64>,
    pub(crate) sessions: usize,
    /// `0` when there is no maximum.
    pub(crate) max_sessions: u32,
    pub(crate) state: HostState,
}

/// One provider's worst limit, or one limit in the usage table.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Limit {
    pub(crate) provider: String,
    pub(crate) label: String,
    pub(crate) percent: Option<f64>,
    pub(crate) reset_ms: Option<i64>,
    /// The quota gate holds new sessions on this provider.
    pub(crate) held: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeadState {
    Working,
    Idle,
    /// Waiting for an answer or an approval.
    Blocked,
    Error,
    Starting,
    Stopped,
    Held,
    Off,
}

/// The lead and the lanes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct People {
    pub(crate) lead: LeadState,
    pub(crate) model: Option<String>,
    pub(crate) running: usize,
    pub(crate) waiting: usize,
    /// The model (`provider/id`, or the requested selector until OMP names one) of every live
    /// session the hub runs, the lead's first. Its length is the count the session cap holds.
    pub(crate) models: Vec<String>,
    /// The quota gate holds the lead's new session: its limits and reset times.
    pub(crate) lead_quota: Option<crate::jobs::owner::quota::Held>,
}

impl Default for People {
    fn default() -> Self {
        Self {
            lead: LeadState::Off,
            model: None,
            running: 0,
            waiting: 0,
            models: Vec::new(),
            lead_quota: None,
        }
    }
}

/// A job that is not fine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Issue {
    pub(crate) job: String,
    /// Failed runs or ticks in a row.
    pub(crate) failures: i64,
    /// Held after a broken run until the owner acknowledges it.
    pub(crate) needs_ack: bool,
}

/// One line of the recent list: an event as stored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Recent {
    pub(crate) at_ms: i64,
    pub(crate) kind: String,
    pub(crate) job: String,
    #[serde(default)]
    pub(crate) fields: Option<String>,
    pub(crate) text: String,
}

/// Everything the panel shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Panel {
    pub(crate) host: Host,
    /// `None` when no usage data was read.
    pub(crate) limits: Option<Vec<Limit>>,
    /// The providers the quota gate holds, in order.
    pub(crate) held: Vec<String>,
    pub(crate) people: People,
    pub(crate) jobs: usize,
    pub(crate) issues: Vec<Issue>,
    pub(crate) recent: Vec<Recent>,
    pub(crate) muted_until_ms: Option<i64>,
    /// The mute button's length, in minutes.
    pub(crate) mute_minutes: u64,
    /// The panel is not pinned because the bot may not pin messages.
    pub(crate) pin_refused: bool,
}

/// Stands for the update time in a rendered panel; [`stamp`] puts the time in its place when the
/// message is sent, so an unchanged panel renders the same body.
const UPDATED_AT: &str = "\u{1}updated\u{1}";

/// Puts the update time `now` on a message body's first embed when it has a footer: the embed's
/// timestamp, which Discord prints after the footer in the reader's zone, and the panel's
/// "updated" line as a relative time.
pub(crate) fn stamp(body: &mut Value, now: i64) {
    let Some(embed) = body.pointer_mut("/embeds/0") else {
        return;
    };
    let Ok(at) = jiff::Timestamp::from_millisecond(now) else {
        return;
    };
    if embed.get("footer").is_none() {
        return;
    }
    set(embed, "timestamp", json!(at.to_string()));
    if let Some(d) = embed.get("description").and_then(Value::as_str)
        && d.contains(UPDATED_AT)
    {
        let d = d.replace(UPDATED_AT, &when(now, 'R'));
        set(embed, "description", json!(d));
    }
}

/// The footer: the version, after the unpinned note while the bot may not pin. Discord shows the
/// embed's timestamp after it.
fn footer(pin_refused: bool, lang: Language, version: &str) -> String {
    let tail = format!("Cataly {version}");
    if pin_refused {
        let note = t(
            lang,
            "고정 안 됨: 봇에 메시지 고정 권한이 없어요",
            "Not pinned: the bot has no permission to pin messages",
        );
        format!("{note}\n{tail}")
    } else {
        tail
    }
}

/// Korean or English.
fn t(lang: Language, ko: &'static str, en: &'static str) -> &'static str {
    match lang {
        Language::Korean => ko,
        Language::English => en,
    }
}

/// A Discord timestamp the reader's client shows in its own time and words (`R`: relative, `t`:
/// short time, `f`: date and time).
pub(crate) fn when(ms: i64, style: char) -> String {
    format!("<t:{}:{style}>", ms.div_euclid(1000))
}

/// A text bar of eight cells for a share in per cent.
pub(crate) fn bar(percent: f64) -> String {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let full = ((percent.clamp(0.0, 100.0) / 100.0) * BAR_CELLS as f64).round() as usize;
    let full = full.min(BAR_CELLS);
    format!("`{}{}`", "▰".repeat(full), "▱".repeat(BAR_CELLS - full))
}

/// A share rounded to whole per cent; values that move every tick are rounded to 5 so the panel is
/// not edited for noise.
fn percent(p: f64, step: f64) -> String {
    format!("{:.0}%", (p / step).round() * step)
}

/// Cuts to `max` characters, ending a cut with an ellipsis.
pub(crate) fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Plain text for inside an embed: markdown characters are escaped, control characters flattened.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '`' | '*' | '_' | '~' | '|' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

impl Panel {
    /// Red only when the owner must act: the host overloaded, a job failing or waiting for an ack,
    /// the lead held, or a held provider a live session uses. A held provider no live session uses
    /// does not raise the level (its info line says sessions on the other providers start
    /// normally), nor does its spent limit.
    pub(crate) fn level(&self) -> Level {
        let failing = self.issues.iter().any(|i| i.failures >= 2 || i.needs_ack);
        let (used, _) = self.held_split();
        if self.host.state == HostState::Alert
            || failing
            || !used.is_empty()
            || self.people.lead == LeadState::Held
        {
            return Level::Act;
        }
        let high = self
            .limits
            .iter()
            .flatten()
            .any(|l| !l.held && l.percent.is_some_and(|p| p >= HIGH_LIMIT));
        if self.host.state == HostState::Busy
            || high
            || !self.issues.is_empty()
            || self.people.lead == LeadState::Error
        {
            Level::Watch
        } else {
            Level::Fine
        }
    }

    /// The held providers a live session uses, then the others. A model without a provider may
    /// use any of them, as the quota gate judges a start.
    fn held_split(&self) -> (Vec<&str>, Vec<&str>) {
        self.held.iter().map(String::as_str).partition(|held| {
            self.people.models.iter().any(|m| match m.split_once('/') {
                Some((provider, _)) if !provider.is_empty() => provider == *held,
                _ => true,
            })
        })
    }

    /// The summary's held-provider line, when the gate holds one.
    fn held_line(&self, lang: Language) -> Option<String> {
        if self.held.is_empty() {
            return None;
        }
        let names = |list: &[&str]| list.iter().map(|p| plain(p)).collect::<Vec<_>>().join(", ");
        let all: Vec<&str> = self.held.iter().map(String::as_str).collect();
        let (used, idle) = self.held_split();
        let mut line = format!("⛔ {} {}", t(lang, "보류:", "Held:"), names(&all));
        if used.is_empty() {
            line.push_str(t(
                lang,
                " · 다른 제공자의 세션은 정상적으로 시작해요",
                " · sessions on other providers start normally",
            ));
        } else {
            let _ = write!(
                line,
                " · {} {}",
                t(
                    lang,
                    "실행 중인 세션이 쓰는 제공자:",
                    "used by a live session:"
                ),
                names(&used)
            );
            if !idle.is_empty() {
                line.push_str(t(
                    lang,
                    " · 그 밖의 제공자 세션은 정상적으로 시작해요",
                    " · sessions on the other providers start normally",
                ));
            }
        }
        Some(line)
    }
}

fn host_value(h: &Host, lang: Language) -> String {
    let mut out = String::new();
    let state = match h.state {
        HostState::Ready => t(lang, "여유", "ready"),
        HostState::Busy => t(lang, "바쁨", "busy"),
        HostState::Alert => t(lang, "과부하", "overloaded"),
    };
    match h.load {
        Some(l) => {
            let _ = writeln!(
                out,
                "{} {} {} · {state}",
                t(lang, "부하", "Load"),
                bar(l),
                percent(l, 5.0)
            );
        }
        None => {
            let _ = writeln!(out, "{} ? · {state}", t(lang, "부하", "Load"));
        }
    }
    let memory = h
        .memory_free
        .map_or_else(|| "?".to_string(), |m| percent(m, 5.0));
    let disk = h
        .disk_free_gb
        .map_or_else(|| "?".to_string(), |d| format!("{d:.0} GB"));
    let _ = writeln!(
        out,
        "{} {memory} · {} {disk}",
        t(lang, "메모리 여유", "Memory free"),
        t(lang, "디스크 여유", "disk free"),
    );
    let max = if h.max_sessions > 0 {
        format!("/{}", h.max_sessions)
    } else {
        String::new()
    };
    let _ = write!(out, "{} {}{max}", t(lang, "세션", "Sessions"), h.sessions);
    out
}

/// The host on one line, for a pressure alert.
pub(crate) fn host_line(h: &Host, lang: Language) -> String {
    host_value(h, lang).replace('\n', " · ")
}

/// The body of a job alert: the failures in a row and the last reason.
pub(crate) fn job_detail(failures: i64, error: Option<&str>, lang: Language) -> String {
    let mut out = match lang {
        Language::Korean => format!("{failures}회 연속 실패했어요."),
        Language::English => format!("Failed {failures} times in a row."),
    };
    if let Some(e) = error.filter(|e| !e.trim().is_empty()) {
        let _ = write!(out, "\n{}", plain(&cut(e, 300)));
    }
    out
}

fn limit_line(l: &Limit, lang: Language, label: bool) -> String {
    let mut line = format!("**{}**", plain(&l.provider));
    if label && !l.label.is_empty() {
        let _ = write!(line, " · {}", plain(&l.label));
    }
    match l.percent {
        Some(p) => {
            let _ = write!(line, " {} {}", bar(p), percent(p, 1.0));
        }
        None => line.push_str(" ?"),
    }
    if let Some(r) = l.reset_ms {
        let _ = write!(line, " · {} {}", t(lang, "리셋", "resets"), when(r, 'R'));
    }
    if l.held {
        let _ = write!(line, " · ⛔ {}", t(lang, "보류", "held"));
    }
    line
}

fn usage_value(limits: Option<&[Limit]>, lang: Language) -> String {
    let Some(limits) = limits else {
        return t(lang, "사용량 정보 없음", "no usage data").to_string();
    };
    if limits.is_empty() {
        return t(lang, "보고된 한도 없음", "no limits reported").to_string();
    }
    let mut lines: Vec<String> = limits
        .iter()
        .take(PROVIDERS_SHOWN)
        .map(|l| limit_line(l, lang, false))
        .collect();
    if limits.len() > PROVIDERS_SHOWN {
        lines.push(match lang {
            Language::Korean => format!("외 {}개", limits.len() - PROVIDERS_SHOWN),
            Language::English => format!("{} more", limits.len() - PROVIDERS_SHOWN),
        });
    }
    lines.join("\n")
}

fn people_value(p: &People, lang: Language) -> String {
    let lead = match p.lead {
        LeadState::Working => t(lang, "작업 중", "working"),
        LeadState::Idle => t(lang, "대기", "idle"),
        LeadState::Blocked => t(lang, "답 기다림", "waiting for you"),
        LeadState::Error => t(lang, "오류", "error"),
        LeadState::Starting => t(lang, "시작 중", "starting"),
        LeadState::Stopped => t(lang, "멈춤", "stopped"),
        LeadState::Held => t(lang, "보류", "held"),
        LeadState::Off => t(lang, "꺼짐", "off"),
    };
    let model = p
        .model
        .as_deref()
        .map_or_default(|m| format!(" · {}", plain(m.rsplit('/').next().unwrap_or(m))));
    // A lead the quota gate holds says which limits, when they reset, and the way out.
    let quota = p.lead_quota.as_ref().map_or_default(|held| {
        let limits = held
            .causes
            .iter()
            .map(|c| {
                let (limit, at) = (plain(&c.limit), when(c.release_ms, 'R'));
                match lang {
                    Language::Korean => format!("{limit} {at} 초기화"),
                    Language::English => format!("{limit} resets {at}"),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        match lang {
            Language::Korean => format!(" · 사용량 한도: {limits} · `cataly quota lift`로 풀어요"),
            Language::English => format!(" · quota gate: {limits} · `cataly quota lift` lifts it"),
        }
    });
    match lang {
        Language::Korean => format!(
            "리드: {lead}{model}{quota}\n레인: 실행 {} · 대기 {}",
            p.running, p.waiting
        ),
        Language::English => format!(
            "Lead: {lead}{model}{quota}\nLanes: {} running · {} waiting",
            p.running, p.waiting
        ),
    }
}

fn jobs_value(jobs: usize, issues: &[Issue], lang: Language) -> String {
    if issues.is_empty() {
        return match lang {
            Language::Korean => format!("✅ 모두 정상 ({jobs}개)"),
            Language::English => format!("✅ all fine ({jobs})"),
        };
    }
    let mut lines: Vec<String> = issues
        .iter()
        .take(ISSUES_SHOWN)
        .map(|i| {
            let job = plain(&i.job);
            match (lang, i.needs_ack) {
                (Language::Korean, true) => format!("⏸️ {job}: 확인 필요"),
                (Language::English, true) => format!("⏸️ {job}: needs your ack"),
                (Language::Korean, false) => format!("⚠️ {job}: {}회 연속 실패", i.failures),
                (Language::English, false) => format!("⚠️ {job}: failed {} in a row", i.failures),
            }
        })
        .collect();
    if issues.len() > ISSUES_SHOWN {
        lines.push(match lang {
            Language::Korean => format!("외 {}개", issues.len() - ISSUES_SHOWN),
            Language::English => format!("{} more", issues.len() - ISSUES_SHOWN),
        });
    }
    lines.join("\n")
}

fn recent_value(recent: &[Recent], lang: Language) -> String {
    if recent.is_empty() {
        return t(lang, "아직 없음", "nothing yet").to_string();
    }
    recent
        .iter()
        .rev()
        .map(|r| format!("• {} · {}", cut(&line(r, lang), 160), when(r.at_ms, 'R')))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The panel message with [`UPDATED_AT`] for its update time (the messenger fills it in), so equal
/// bodies mean nothing changed.
pub(crate) fn panel(p: &Panel, lang: Language, version: &str) -> Value {
    let (colour, summary) = match p.level() {
        Level::Fine => (GREEN, t(lang, "🟢 모두 정상", "🟢 All fine")),
        Level::Watch => (AMBER, t(lang, "🟡 살펴볼 것이 있어요", "🟡 Worth a look")),
        Level::Act => (RED, t(lang, "🔴 확인이 필요해요", "🔴 Needs you")),
    };
    let mut description = summary.to_string();
    if let Some(held) = p.held_line(lang) {
        let _ = write!(description, "\n{held}");
    }
    if let Some(until) = p.muted_until_ms {
        let _ = write!(
            description,
            "\n🔕 {} {}",
            t(lang, "알림 꺼짐 ·", "Alerts muted until"),
            when(until, 't')
        );
        if lang == Language::Korean {
            description.push_str("까지");
        }
    }
    let _ = write!(
        description,
        "\n🕒 {} {UPDATED_AT}",
        t(lang, "마지막 갱신", "Updated")
    );
    let field = |name: &str, value: String, inline: bool| json!({ "name": cut(name, FIELD_NAME_MAX), "value": cut(&value, FIELD_VALUE_MAX), "inline": inline });
    let fields = vec![
        field(
            t(lang, "🖥️ 서버", "🖥️ Host"),
            host_value(&p.host, lang),
            true,
        ),
        field(
            t(lang, "👥 세션", "👥 Sessions"),
            people_value(&p.people, lang),
            true,
        ),
        field(
            t(lang, "🔋 사용량", "🔋 Usage"),
            usage_value(p.limits.as_deref(), lang),
            false,
        ),
        field(
            t(lang, "🛠️ 작업", "🛠️ Jobs"),
            jobs_value(p.jobs, &p.issues, lang),
            false,
        ),
        field(
            t(lang, "📮 최근", "📮 Recent"),
            recent_value(&p.recent, lang),
            false,
        ),
    ];
    let embed = json!({
        "title": t(lang, "Cataly 상황판", "Cataly status"),
        "description": cut(&description, DESCRIPTION_MAX),
        "color": colour,
        "thumbnail": { "url": format!("attachment://{THUMB}") },
        "image": { "url": format!("attachment://{BANNER}") },
        "fields": fields,
        "footer": { "text": cut(&footer(p.pin_refused, lang, version), FOOTER_MAX) },
    });
    let mute = if p.muted_until_ms.is_some() {
        button(t(lang, "알림 다시 켜기", "Unmute alerts"), "🔔", MUTE)
    } else {
        let label = match lang {
            Language::Korean => format!("{} 조용히", minutes_words(p.mute_minutes, lang)),
            Language::English => format!("Mute {}", minutes_words(p.mute_minutes, lang)),
        };
        button(&label, "🔕", MUTE)
    };
    json!({
        "embeds": [fit(embed)],
        "components": [{ "type": 1, "components": [
            button(t(lang, "새로고침", "Refresh"), "🔄", REFRESH),
            button(t(lang, "사용량 자세히", "Usage details"), "📊", USAGE),
            mute,
            button(t(lang, "감시 채널", "Watched channels"), "📡", WATCH),
        ]}],
    })
}

fn button(label: &str, emoji: &str, id: &str) -> Value {
    json!({ "type": 2, "style": 2, "label": cut(label, 80), "emoji": { "name": emoji }, "custom_id": id })
}

/// `1시간`, `30분`, `1h30m` style words for a length in minutes.
pub(crate) fn minutes_words(minutes: u64, lang: Language) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    match (lang, h, m) {
        (Language::Korean, 0, m) => format!("{m}분"),
        (Language::Korean, h, 0) => format!("{h}시간"),
        (Language::Korean, h, m) => format!("{h}시간 {m}분"),
        (Language::English, 0, m) => format!("{m} min"),
        (Language::English, 1, 0) => "1 hour".into(),
        (Language::English, h, 0) => format!("{h} hours"),
        (Language::English, h, m) => format!("{h}h{m}m"),
    }
}

/// Keeps an embed within Discord's limits: each part cut to its own limit, then the longest field
/// values shortened until the whole fits.
pub(crate) fn fit(mut embed: Value) -> Value {
    if let Some(title) = text_of(&embed, "title") {
        set(&mut embed, "title", json!(cut(&title, TITLE_MAX)));
    }
    loop {
        let total = embed_size(&embed);
        if total <= EMBED_TOTAL_MAX {
            return embed;
        }
        let Some(fields) = embed.get_mut("fields").and_then(Value::as_array_mut) else {
            let d = text_of(&embed, "description").unwrap_or_default();
            let keep = d.chars().count().saturating_sub(total - EMBED_TOTAL_MAX);
            set(&mut embed, "description", json!(cut(&d, keep)));
            return embed;
        };
        let Some(longest) = fields
            .iter_mut()
            .max_by_key(|f| text_of(f, "value").map_or(0, |v| v.chars().count()))
        else {
            return embed;
        };
        let value = text_of(longest, "value").unwrap_or_default();
        let len = value.chars().count();
        let keep = len.saturating_sub(total - EMBED_TOTAL_MAX).max(1);
        set(longest, "value", json!(cut(&value, keep)));
        if keep == 1 && len <= 1 {
            return embed;
        }
    }
}

fn text_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

pub(crate) fn set(v: &mut Value, key: &str, value: Value) {
    if let Some(map) = v.as_object_mut() {
        map.insert(key.to_string(), value);
    }
}

/// Characters Discord counts toward an embed's 6000.
pub(crate) fn embed_size(embed: &Value) -> usize {
    let len = |v: Option<&Value>| v.and_then(Value::as_str).map_or(0, |s| s.chars().count());
    let mut total = len(embed.get("title"))
        + len(embed.get("description"))
        + len(embed.pointer("/footer/text"));
    let fields = embed.get("fields").and_then(Value::as_array);
    for f in fields.into_iter().flatten() {
        total += len(f.get("name")) + len(f.get("value"));
    }
    total
}

/// The usage table for the usage button: every limit with its bar, reset and hold.
pub(crate) fn usage_table(
    limits: Option<&[Limit]>,
    fetched_ms: Option<i64>,
    lang: Language,
) -> Value {
    let mut text = match limits {
        None => t(lang, "사용량 정보가 아직 없어요.", "No usage data yet.").to_string(),
        Some([]) => t(lang, "보고된 한도가 없어요.", "No limits reported.").to_string(),
        Some(list) => list
            .iter()
            .map(|l| limit_line(l, lang, true))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    if let Some(at) = fetched_ms {
        let _ = write!(
            text,
            "\n\n{} {}",
            t(lang, "읽은 시각", "Read"),
            when(at, 'R')
        );
    }
    let embed = json!({
        "title": t(lang, "📊 사용량 자세히", "📊 Usage details"),
        "description": cut(&text, DESCRIPTION_MAX),
        "color": GREY,
    });
    json!({ "type": 4, "data": { "embeds": [fit(embed)], "flags": 64 } })
}

/// An ephemeral answer to a button press.
pub(crate) fn private(text: &str) -> Value {
    json!({ "type": 4, "data": { "content": cut(text, 2000), "flags": 64, "allowed_mentions": { "parse": [] } } })
}

/// The owner's picker of watched channels: Discord's channel select (text and forum channels)
/// with the first 25 watched channels chosen, ephemeral.
pub(crate) fn watch_picker(watched: &[crate::relay::discord::Snowflake], lang: Language) -> Value {
    let defaults: Vec<Value> = watched
        .iter()
        .take(PICKER_MAX)
        .map(|c| json!({ "id": c.to_string(), "type": "channel" }))
        .collect();
    let rest = watched.len().saturating_sub(PICKER_MAX);
    let mut text = t(
        lang,
        "📡 Cataly가 지켜볼 채널을 고르세요.",
        "📡 Choose the channels Cataly watches.",
    )
    .to_string();
    if rest > 0 {
        let _ = match lang {
            Language::Korean => write!(
                text,
                " 목록에 다 담기지 않는 {rest}개 채널은 그대로 지켜봐요."
            ),
            Language::English => write!(
                text,
                " The other {rest} watched channels do not fit in the list and stay watched."
            ),
        };
    }
    json!({ "type": 4, "data": {
        "content": text,
        "flags": 64,
        "allowed_mentions": { "parse": [] },
        "components": [{ "type": 1, "components": [{
            "type": 8,
            "custom_id": WATCHED,
            "channel_types": [0, 15],
            "min_values": 1,
            "max_values": PICKER_MAX,
            "default_values": defaults,
        }]}],
    }})
}

/// The picker's answer once the list is applied: the watched channels as mentions, in place of
/// the picker. `kept` channels the picker could not show stay watched.
pub(crate) fn watching(
    channels: &[crate::relay::discord::Snowflake],
    kept: usize,
    lang: Language,
) -> Value {
    let mentions: Vec<String> = channels
        .iter()
        .take(PICKER_MAX)
        .map(|c| format!("<#{c}>"))
        .collect();
    let mut text = format!(
        "{} {}",
        t(lang, "📡 지켜보는 채널:", "📡 Watching:"),
        mentions.join(" ")
    );
    let more = channels.len().saturating_sub(PICKER_MAX);
    if more > 0 {
        let _ = match lang {
            Language::Korean => write!(text, " 외 {more}개"),
            Language::English => write!(text, " and {more} more"),
        };
    }
    if kept > 0 {
        let _ = match lang {
            Language::Korean => write!(text, " (목록에 없던 {kept}개 채널은 그대로 둬요)"),
            Language::English => {
                write!(text, " ({kept} channels the list could not show are kept)")
            }
        };
    }
    replace(&text)
}

/// The picker's answer when nothing changed.
pub(crate) fn watch_failed(why: &str, lang: Language) -> Value {
    let head = t(
        lang,
        "⚠️ 감시 채널을 바꾸지 못했어요. 지금 설정을 그대로 써요:",
        "⚠️ The watched channels were not changed; the running config stays:",
    );
    replace(&format!("{head} {why}"))
}

/// Replaces the ephemeral message a component sits on with one line and no components.
fn replace(text: &str) -> Value {
    json!({ "type": 7, "data": { "content": cut(text, 2000), "components": [], "allowed_mentions": { "parse": [] } } })
}

/// An open alert: a small embed with the thumbnail.
pub(crate) fn alert(
    title: &str,
    detail: &str,
    opened_ms: i64,
    level: Level,
    lang: Language,
) -> Value {
    let colour = if level == Level::Act { RED } else { AMBER };
    let embed = json!({
        "title": cut(title, TITLE_MAX),
        "description": cut(detail, DESCRIPTION_MAX),
        "color": colour,
        "thumbnail": { "url": format!("attachment://{THUMB}") },
        "fields": [{ "name": t(lang, "시작", "Since"), "value": format!("{} ({})", when(opened_ms, 't'), when(opened_ms, 'R')), "inline": true }],
    });
    json!({ "embeds": [fit(embed)] })
}

/// The same alert, resolved: edited in place instead of a new post.
pub(crate) fn resolved(
    title: &str,
    detail: &str,
    opened_ms: i64,
    resolved_ms: i64,
    lang: Language,
) -> Value {
    let embed = json!({
        "title": cut(&format!("✅ {} · {title}", t(lang, "해결됨", "Resolved")), TITLE_MAX),
        "description": cut(detail, DESCRIPTION_MAX),
        "color": GREEN,
        "thumbnail": { "url": format!("attachment://{THUMB}") },
        "fields": [
            { "name": t(lang, "시작", "Since"), "value": when(opened_ms, 't'), "inline": true },
            { "name": t(lang, "해결", "Resolved"), "value": format!("{} ({})", when(resolved_ms, 't'), when(resolved_ms, 'R')), "inline": true },
        ],
    });
    json!({ "embeds": [fit(embed)] })
}

fn get(r: &Recent, name: &str) -> String {
    plain(&field(r.fields.as_deref(), name).unwrap_or_default())
}

/// The providers of quota limit keys (`provider#account/id`), each once.
pub(crate) fn providers(limits: &str) -> String {
    let mut out: Vec<&str> = limits
        .split(", ")
        .map(|l| l.split_once('#').map_or(l, |(p, _)| p))
        .filter(|p| !p.is_empty())
        .collect();
    out.dedup();
    plain(&out.join(", "))
}

/// The short line of an event: the alert title for alert kinds, a few words for the rest, the
/// event's own text for kinds without words here.
#[allow(clippy::too_many_lines)]
pub(crate) fn line(r: &Recent, lang: Language) -> String {
    let job = plain(&r.job);
    let ko = lang == Language::Korean;
    let pick = |k: String, e: String| if ko { k } else { e };
    match r.kind.as_str() {
        "pressure.alert" => t(
            lang,
            "🔥 서버 부하가 5분 넘게 높아요",
            "🔥 Host under pressure for 5+ minutes",
        )
        .into(),
        "pressure.clear" => t(lang, "✅ 서버 부하 정상", "✅ Host pressure cleared").into(),
        "quota.hold" => {
            let p = providers(&get(r, "limits"));
            pick(
                format!("⛔ 사용량 한도: {p} 새 세션 보류"),
                format!("⛔ Quota hold: new {p} sessions wait"),
            )
        }
        "quota.released" => t(lang, "✅ 사용량 보류 풀림", "✅ Quota hold lifted").into(),
        "quota.level" => {
            let (p, pc) = (get(r, "provider"), get(r, "percent"));
            pick(
                format!("🔋 {p} 사용량 {pc}%"),
                format!("🔋 {p} usage at {pc}%"),
            )
        }
        "quota.recovered" => {
            let (p, pc) = (get(r, "provider"), get(r, "percent"));
            pick(
                format!("🔋 {p} 사용량 {pc}%로 내려감"),
                format!("🔋 {p} usage back to {pc}%"),
            )
        }
        "job.failed" => pick(
            format!("🛠️ 작업 실패: {job}"),
            format!("🛠️ Job failing: {job}"),
        ),
        "job.recovered" => pick(
            format!("✅ 작업 복구: {job}"),
            format!("✅ Job works again: {job}"),
        ),
        "run.broken" => pick(
            format!("⏸️ {job}: 실행이 끊겨 보류됨"),
            format!("⏸️ {job}: run broken off, on hold"),
        ),
        "probe.error" => pick(
            format!("⚠️ {job}: 확인 오류"),
            format!("⚠️ {job}: probe error"),
        ),
        "probe.recovered" => pick(
            format!("✅ {job}: 확인 정상"),
            format!("✅ {job}: probe works again"),
        ),
        "backup.refused" => {
            let rule = get(r, "rule");
            pick(
                format!("🗄️ 백업 거부됨 ({rule})"),
                format!("🗄️ Backup refused ({rule})"),
            )
        }
        "backup.clean" => t(lang, "✅ 백업 정상", "✅ Backup clean again").into(),
        "backup.push_failed" => t(lang, "🗄️ 백업 푸시 실패", "🗄️ Backup push failed").into(),
        "health.problem" => {
            let c = get(r, "check");
            pick(format!("🩺 점검 문제: {c}"), format!("🩺 Health: {c}"))
        }
        "health.cleared" => {
            let c = get(r, "check");
            pick(format!("✅ 점검 정상: {c}"), format!("✅ Health fine: {c}"))
        }
        "github.pr" => {
            let (repo, n) = (get(r, "repo"), get(r, "number"));
            pick(
                format!("🐙 새 PR {repo}#{n}"),
                format!("🐙 New PR {repo}#{n}"),
            )
        }
        "github.notification" => {
            let (repo, reason) = (get(r, "repo"), get(r, "reason"));
            pick(
                format!("🔔 GitHub {repo}: {reason}"),
                format!("🔔 GitHub {repo}: {reason}"),
            )
        }
        "pr.feedback" => {
            let (pr, who) = (get(r, "pr"), get(r, "author"));
            pick(
                format!("💬 {pr}: {who}의 피드백"),
                format!("💬 {pr}: feedback from {who}"),
            )
        }
        "pr.check_failed" => {
            let (pr, check) = (get(r, "pr"), get(r, "check"));
            pick(
                format!("❌ {pr}: 체크 {check} 실패"),
                format!("❌ {pr}: check {check} failed"),
            )
        }
        "pr.merged" => {
            let pr = get(r, "pr");
            pick(format!("🟣 {pr} 병합됨"), format!("🟣 {pr} merged"))
        }
        "pr.closed" => {
            let pr = get(r, "pr");
            pick(format!("⚪ {pr} 닫힘"), format!("⚪ {pr} closed"))
        }
        "items.new" => pick(
            format!("📬 새 소식 · {job}"),
            format!("📬 New item · {job}"),
        ),
        _ => plain(&r.text),
    }
}

/// The body under an alert's title, from the event's fields.
pub(crate) fn detail(r: &Recent, lang: Language) -> String {
    let ko = lang == Language::Korean;
    match r.kind.as_str() {
        "backup.refused" => {
            let (rule, files) = (get(r, "rule"), get(r, "files"));
            if ko {
                format!("아무것도 커밋하지 않았어요.\n규칙: {rule}\n파일: {files}")
            } else {
                format!("Nothing was committed.\nRule: {rule}\nFiles: {files}")
            }
        }
        "github.pr" => {
            let (title, author, url) = (get(r, "title"), get(r, "author"), get(r, "url"));
            format!("{title}\n{author} · {url}")
        }
        "github.notification" => {
            let (title, kind) = (get(r, "title"), get(r, "kind"));
            format!("{kind}: {title}")
        }
        "pr.feedback" => {
            let (what, excerpt, url) = (get(r, "what"), get(r, "excerpt"), get(r, "url"));
            format!("{what}: {excerpt}\n{url}")
        }
        "pr.check_failed" => {
            let (conclusion, head) = (get(r, "conclusion"), get(r, "head"));
            format!("{conclusion} · {head}")
        }
        "pr.merged" => get(r, "commit"),
        "items.new" => get(r, "title"),
        _ => String::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> Panel {
        Panel {
            host: Host {
                load: Some(38.0),
                memory_free: Some(62.0),
                disk_free_gb: Some(210.4),
                sessions: 3,
                max_sessions: 20,
                state: HostState::Ready,
            },
            limits: Some(vec![
                Limit {
                    provider: "anthropic".into(),
                    label: "5h".into(),
                    percent: Some(64.0),
                    reset_ms: Some(1_800_000_000_000),
                    held: false,
                },
                Limit {
                    provider: "openai-codex".into(),
                    label: "weekly".into(),
                    percent: Some(97.0),
                    reset_ms: None,
                    held: true,
                },
            ]),
            held: vec!["openai-codex".into()],
            people: People {
                lead: LeadState::Working,
                model: Some("anthropic/claude-opus-5-5".into()),
                running: 2,
                waiting: 1,
                models: vec![
                    "anthropic/claude-opus-5-5".into(),
                    "openai-codex/gpt-5.5".into(),
                    "anthropic/claude-sonnet-5".into(),
                ],
                lead_quota: None,
            },
            jobs: 5,
            issues: Vec::new(),
            recent: vec![Recent {
                at_ms: 1_700_000_000_000,
                kind: "github.pr".into(),
                job: "watch".into(),
                fields: Some(r#"{"repo":"me/app","number":"12"}"#.into()),
                text: "new PR me/app#12".into(),
            }],
            muted_until_ms: None,
            mute_minutes: 60,
            pin_refused: false,
        }
    }

    #[test]
    fn korean_and_english_panels() {
        let ko = panel(&sample(), Language::Korean, "0.1.1");
        let embed = &ko["embeds"][0];
        assert_eq!(embed["title"], "Cataly 상황판");
        assert_eq!(embed["color"], RED, "a held provider needs the owner");
        assert_eq!(embed["image"]["url"], "attachment://panel-banner.jpg");
        assert_eq!(embed["thumbnail"]["url"], "attachment://panel-thumb.jpg");
        let text = embed.to_string();
        for want in [
            "🖥️ 서버",
            "부하 `▰▰▰▱▱▱▱▱` 40%",
            "세션 3/20",
            "⛔ 보류",
            "리드: 작업 중 · claude-opus-5-5",
            "레인: 실행 2 · 대기 1",
            "✅ 모두 정상 (5개)",
            "🐙 새 PR me/app#12 · <t:1700000000:R>",
            "<t:1800000000:R>",
        ] {
            assert!(text.contains(want), "{want} in {text}");
        }
        let labels: Vec<&str> = ko["components"][0]["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["label"].as_str().unwrap())
            .collect();
        assert_eq!(
            labels,
            ["새로고침", "사용량 자세히", "1시간 조용히", "감시 채널"]
        );
        assert!(
            embed.get("timestamp").is_none(),
            "the time is added when sent"
        );
        let en = panel(&sample(), Language::English, "0.1.1");
        let text = en.to_string();
        for want in [
            "Cataly status",
            "🖥️ Host",
            "Lead: working",
            "2 running · 1 waiting",
            "✅ all fine (5)",
            "Mute 1 hour",
            "🐙 New PR me/app#12",
        ] {
            assert!(text.contains(want), "{want} in {text}");
        }
        let mut calm = sample();
        calm.limits = Some(Vec::new());
        calm.held.clear();
        assert_eq!(
            panel(&calm, Language::Korean, "x")["embeds"][0]["color"],
            GREEN
        );
        calm.host.state = HostState::Busy;
        assert_eq!(
            panel(&calm, Language::Korean, "x")["embeds"][0]["color"],
            AMBER
        );
        calm.muted_until_ms = Some(1_700_000_360_000);
        let muted = panel(&calm, Language::Korean, "x");
        assert!(
            muted
                .to_string()
                .contains("🔕 알림 꺼짐 · <t:1700000360:t>까지")
        );
        assert_eq!(
            muted["components"][0]["components"][2]["label"],
            "알림 다시 켜기"
        );
    }

    #[test]
    fn embeds_stay_within_discord_limits() {
        let mut big = sample();
        big.limits = Some(
            (0..40)
                .map(|i| Limit {
                    provider: format!("provider-{i}-{}", "x".repeat(200)),
                    label: "y".repeat(300),
                    percent: Some(50.0),
                    reset_ms: Some(1_800_000_000_000),
                    held: i % 2 == 0,
                })
                .collect(),
        );
        big.issues = (0..10)
            .map(|i| Issue {
                job: format!("job-{i}-{}", "z".repeat(300)),
                failures: 3,
                needs_ack: false,
            })
            .collect();
        big.recent = (0..3)
            .map(|i| Recent {
                at_ms: i,
                kind: "other".into(),
                job: "j".into(),
                fields: None,
                text: "w".repeat(500),
            })
            .collect();
        for lang in [Language::Korean, Language::English] {
            let body = panel(&big, lang, "0.1.1");
            let embed = &body["embeds"][0];
            assert!(embed_size(embed) <= EMBED_TOTAL_MAX);
            for f in embed["fields"].as_array().unwrap() {
                assert!(f["value"].as_str().unwrap().chars().count() <= FIELD_VALUE_MAX);
                assert!(f["name"].as_str().unwrap().chars().count() <= FIELD_NAME_MAX);
            }
            assert!(embed["fields"].as_array().unwrap().len() <= 25);
            let table = usage_table(big.limits.as_deref(), Some(1), lang);
            let e = &table["data"]["embeds"][0];
            assert!(e["description"].as_str().unwrap().chars().count() <= DESCRIPTION_MAX);
            assert_eq!(table["data"]["flags"], 64);
        }
        // A total over 6000 shortens the longest values until it fits.
        let fat = json!({ "title": "t", "description": "d".repeat(3000), "fields": [
            { "name": "a", "value": "a".repeat(1024) }, { "name": "b", "value": "b".repeat(1024) },
            { "name": "c", "value": "c".repeat(1024) }, { "name": "d", "value": "d".repeat(1024) }] });
        assert!(embed_size(&fit(fat)) <= EMBED_TOTAL_MAX);
        let alert = alert(
            &"t".repeat(400),
            &"d".repeat(5000),
            0,
            Level::Act,
            Language::Korean,
        );
        assert!(embed_size(&alert["embeds"][0]) <= EMBED_TOTAL_MAX);
        assert!(
            alert["embeds"][0]["title"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= TITLE_MAX
        );
    }

    #[test]
    fn words_and_bars() {
        assert_eq!(bar(0.0), "`▱▱▱▱▱▱▱▱`");
        assert_eq!(bar(100.0), "`▰▰▰▰▰▰▰▰`");
        assert_eq!(bar(250.0), "`▰▰▰▰▰▰▰▰`");
        assert_eq!(minutes_words(60, Language::Korean), "1시간");
        assert_eq!(minutes_words(90, Language::Korean), "1시간 30분");
        assert_eq!(minutes_words(30, Language::English), "30 min");
        assert_eq!(providers("a#1/x, a#2/y, b#1/z"), "a, b");
        assert_eq!(plain("a_b `x`\n"), "a\\_b \\`x\\` ");
        let r = Recent {
            at_ms: 0,
            kind: "items.new".into(),
            job: "gsc".into(),
            fields: Some(r#"{"title":"New issue on your site"}"#.into()),
            text: "gsc: New issue on your site".into(),
        };
        assert_eq!(line(&r, Language::Korean), "📬 새 소식 · gsc");
        assert_eq!(line(&r, Language::English), "📬 New item · gsc");
        assert_eq!(detail(&r, Language::Korean), "New issue on your site");
        let done = resolved("🔥 서버", "부하 172%", 0, 600_000, Language::Korean);
        assert!(
            done["embeds"][0]["title"]
                .as_str()
                .unwrap()
                .starts_with("✅ 해결됨 · ")
        );
        assert_eq!(done["embeds"][0]["color"], GREEN);
    }

    /// The live panel of 0.1.4: the lead alone on anthropic, `openai-codex` and `devin` held at
    /// 100 %, six jobs all fine.
    fn held_elsewhere() -> Panel {
        let mut p = sample();
        p.held = vec!["devin".into(), "openai-codex".into()];
        let limit = |provider: &str, percent: f64, held: bool| {
            let mut l = p.limits.iter().flatten().next().cloned().unwrap();
            l.provider = provider.into();
            l.percent = Some(percent);
            l.held = held;
            l
        };
        p.limits = Some(vec![
            limit("openai-codex", 100.0, true),
            limit("anthropic", 40.0, false),
            limit("devin", 100.0, true),
        ]);
        p.jobs = 6;
        p.issues.clear();
        p.people.running = 0;
        p.people.waiting = 0;
        p.people.models = vec!["anthropic/claude-opus-5-5".into()];
        p.host.sessions = p.people.models.len();
        p
    }

    fn description(p: &Panel, lang: Language) -> String {
        panel(p, lang, "x")["embeds"][0]["description"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn a_held_provider_no_live_session_uses_stays_green() {
        let p = held_elsewhere();
        assert_eq!(p.level(), Level::Fine);
        let ko = panel(&p, Language::Korean, "x");
        assert_eq!(ko["embeds"][0]["color"], GREEN);
        let d = description(&p, Language::Korean);
        assert!(d.starts_with("🟢 모두 정상\n"), "{d}");
        assert!(
            d.contains("⛔ 보류: devin, openai-codex · 다른 제공자의 세션은 정상적으로 시작해요"),
            "{d}"
        );
        assert!(!d.contains("확인이 필요해요"), "{d}");
        let d = description(&p, Language::English);
        assert!(d.starts_with("🟢 All fine\n"), "{d}");
        assert!(
            d.contains("⛔ Held: devin, openai-codex · sessions on other providers start normally"),
            "{d}"
        );
    }

    #[test]
    fn a_limit_turns_amber_at_ninety_per_cent() {
        let mut p = held_elsewhere();
        let anthropic = |p: &mut Panel, percent: f64| {
            for l in p.limits.iter_mut().flatten() {
                if l.provider == "anthropic" {
                    l.percent = Some(percent);
                }
            }
        };
        anthropic(&mut p, 85.0);
        assert_eq!(p.level(), Level::Fine);
        anthropic(&mut p, 90.0);
        assert_eq!(p.level(), Level::Watch);
        assert_eq!(
            panel(&p, Language::English, "x")["embeds"][0]["color"],
            AMBER
        );
    }

    #[test]
    fn the_held_provider_of_a_live_session_is_red() {
        let mut p = held_elsewhere();
        p.held = vec!["anthropic".into(), "devin".into()];
        assert_eq!(p.level(), Level::Act, "the lead's own provider is held");
        let d = description(&p, Language::Korean);
        assert!(d.starts_with("🔴 확인이 필요해요\n"), "{d}");
        assert!(
            d.contains(
                "⛔ 보류: anthropic, devin · 실행 중인 세션이 쓰는 제공자: anthropic · 그 밖의 제공자 세션은 정상적으로 시작해요"
            ),
            "{d}"
        );
        let d = description(&p, Language::English);
        assert!(
            d.contains(
                "⛔ Held: anthropic, devin · used by a live session: anthropic · sessions on the other providers start normally"
            ),
            "{d}"
        );
        // A lane on a held provider counts as much as the lead.
        let mut p = held_elsewhere();
        p.people.models.push("devin/swe-1".into());
        assert_eq!(p.level(), Level::Act);
        // A lead held by the quota gate or by OMP's usage limit needs the owner.
        let mut p = held_elsewhere();
        p.held.clear();
        p.people.lead = LeadState::Held;
        assert_eq!(p.level(), Level::Act);
    }

    #[test]
    fn sessions_count_the_lead_and_the_time_is_relative() {
        let p = held_elsewhere();
        for (lang, want, updated) in [
            (Language::Korean, "세션 1/20", "🕒 마지막 갱신 "),
            (Language::English, "Sessions 1/20", "🕒 Updated "),
        ] {
            let mut body = panel(&p, lang, "0.1.4");
            assert!(body.to_string().contains(want), "{want}");
            assert_eq!(body["embeds"][0]["footer"]["text"], "Cataly 0.1.4");
            stamp(&mut body, 1_795_660_240_551);
            let embed = &body["embeds"][0];
            let d = embed["description"].as_str().unwrap();
            assert!(d.ends_with(&format!("{updated}<t:1795660240:R>")), "{d}");
            assert_eq!(embed["timestamp"], "2026-11-26T02:30:40.551Z");
        }
        // An alert keeps its text; only the timestamp is added.
        let mut a = alert("t", "d", 0, Level::Watch, Language::Korean);
        let before = a["embeds"][0]["description"].clone();
        stamp(&mut a, 1_795_660_240_551);
        assert_eq!(a["embeds"][0]["description"], before);
    }
}
