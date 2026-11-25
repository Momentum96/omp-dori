//! `[voice]` (VOICE-01, VOICE req. 49-50) and `[transcribe]` (VOICE-24, OJ-32).
//!
//! Voice is off unless `enabled = true`. Any missing or invalid voice setting refuses voice alone
//! and names the field; the rest of the configuration still applies. A build without the `voice`
//! feature refuses `enabled = true` the same way.

#[cfg(feature = "voice")]
use std::path::PathBuf;

#[cfg(feature = "voice")]
use toml::Value;

use super::{ConfigError, Section};

/// Default transcription download cap: 25 MiB (VOICE req. 86).
pub(crate) const TRANSCRIBE_MAX: u64 = 25 * 1024 * 1024;

/// What `[voice]` says.
#[derive(Debug, Clone, Default)]
pub(crate) enum VoiceConfig {
    #[default]
    Off,
    /// Enabled but invalid: voice stays off and these name the fields.
    Refused(Vec<ConfigError>),
    #[cfg(feature = "voice")]
    On(Box<VoiceOn>),
}

impl VoiceConfig {
    /// Why voice is off although enabled, naming each field. A voice build's voice task reports
    /// this itself.
    #[cfg(not(feature = "voice"))]
    pub(crate) fn refusal(&self) -> Option<String> {
        match self {
            Self::Refused(errors) => Some(super::describe(errors)),
            Self::Off => None,
        }
    }
}

/// Valid voice settings and where the provider key lives.
#[cfg(feature = "voice")]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VoiceOn {
    pub(crate) settings: cataly_voice::Settings,
    /// Default `<config dir>/secrets/voice-key`.
    pub(crate) key_file: Option<PathBuf>,
}

/// `[transcribe]`: the owner's command (the file path is appended) and the download cap.
#[derive(Debug, Clone)]
pub(crate) struct TranscribeSettings {
    pub(crate) command: Vec<String>,
    pub(crate) max_bytes: u64,
}

impl Default for TranscribeSettings {
    fn default() -> Self {
        Self {
            command: Vec::new(),
            max_bytes: TRANSCRIBE_MAX,
        }
    }
}

pub(super) fn parse_transcribe(
    errors: &mut Vec<ConfigError>,
    root: &mut Section<'_>,
    out: &mut TranscribeSettings,
) {
    let Some(mut t) = root.table(errors, "transcribe") else {
        return;
    };
    if let Some(argv) = t.strings(errors, "command") {
        if argv.first().is_none_or(String::is_empty) {
            errors.push(t.error("command", "expected a program and its arguments"));
        } else {
            out.command = argv;
        }
    }
    if let Some(mib) = t.integer(errors, "max_mib", 1, 1024) {
        out.max_bytes = u64::try_from(mib).unwrap_or(25) * 1024 * 1024;
    }
    t.finish(errors);
}

/// Reads `[voice]`; `discord` supplies the guild and owner.
pub(super) fn parse(root: &mut Section<'_>, discord: &super::DiscordSettings) -> VoiceConfig {
    let mut errors = Vec::new();
    let Some(mut v) = root.table(&mut errors, "voice") else {
        return if errors.is_empty() {
            VoiceConfig::Off
        } else {
            VoiceConfig::Refused(errors)
        };
    };
    if !v.bool(&mut errors, "enabled").unwrap_or(false) {
        // Off: the other keys are not read, so a half-written section does not matter.
        v.used.extend(v.table.keys().map(String::as_str));
        return VoiceConfig::Off;
    }
    let on = enabled(&mut errors, &mut v, discord);
    v.finish(&mut errors);
    match on {
        #[cfg(feature = "voice")]
        Some(on) if errors.is_empty() => VoiceConfig::On(on),
        _ => VoiceConfig::Refused(errors),
    }
}

#[cfg(not(feature = "voice"))]
fn enabled(
    errors: &mut Vec<ConfigError>,
    v: &mut Section<'_>,
    _discord: &super::DiscordSettings,
) -> Option<Box<()>> {
    errors.push(v.error(
        "enabled",
        "this build has no voice support (build with the `voice` feature)",
    ));
    None
}

/// A number (integer or float) that `ok` accepts.
#[cfg(feature = "voice")]
fn number(
    errors: &mut Vec<ConfigError>,
    s: &mut Section<'_>,
    key: &'static str,
    ok: fn(f64) -> bool,
    expect: &str,
) -> Option<f64> {
    s.used.push(key);
    #[allow(clippy::cast_precision_loss)]
    let n = match s.table.get(key)? {
        Value::Float(f) => *f,
        Value::Integer(i) => *i as f64,
        _ => {
            errors.push(s.error(key, expect));
            return None;
        }
    };
    if ok(n) {
        Some(n)
    } else {
        errors.push(s.error(key, expect));
        None
    }
}

#[cfg(feature = "voice")]
fn required<T>(
    v: Option<T>,
    errors: &mut Vec<ConfigError>,
    s: &Section<'_>,
    key: &str,
) -> Option<T> {
    if v.is_none() && !s.table.contains_key(key) {
        errors.push(s.error(key, "required when voice is enabled"));
    }
    v
}

#[cfg(feature = "voice")]
fn prices(
    errors: &mut Vec<ConfigError>,
    s: &mut Section<'_>,
    key: &str,
) -> Option<cataly_voice::Prices> {
    use cataly_voice::settings::valid_price;
    let table = s.table(errors, key);
    let mut t = required(table, errors, s, key)?;
    let expect = "expected a price in USD per million tokens (0 or more)";
    let one = |errors: &mut Vec<ConfigError>, t: &mut Section<'_>, k: &'static str| {
        let n = number(errors, t, k, valid_price, expect);
        required(n, errors, t, k)
    };
    let input = one(errors, &mut t, "input");
    let cached_input = one(errors, &mut t, "cached_input");
    let cache_write = one(errors, &mut t, "cache_write");
    let output = one(errors, &mut t, "output");
    t.finish(errors);
    Some(cataly_voice::Prices {
        input: input?,
        cached_input: cached_input?,
        cache_write: cache_write?,
        output: output?,
    })
}

#[cfg(feature = "voice")]
#[allow(clippy::too_many_lines)]
fn enabled(
    errors: &mut Vec<ConfigError>,
    v: &mut Section<'_>,
    discord: &super::DiscordSettings,
) -> Option<Box<VoiceOn>> {
    use std::time::Duration;

    use cataly_voice::settings::{defaults, valid_effort, valid_name, valid_price, valid_wake};
    use cataly_voice::{Settings, Transport};

    use crate::relay::discord::Snowflake;

    let snowflake = |errors: &mut Vec<ConfigError>, v: &mut Section<'_>, key: &str| {
        let s = v.string(errors, key);
        let id = s
            .filter(|s| Snowflake::parse(s).is_some())
            .and_then(|s| s.parse::<u64>().ok());
        if s.is_some() && id.is_none() {
            errors.push(v.error(key, "expected a Discord ID"));
        }
        required(id, errors, v, key)
    };
    let name = |errors: &mut Vec<ConfigError>, v: &mut Section<'_>, key: &str| {
        let s = v.string(errors, key).map(str::to_string);
        if s.as_deref().is_some_and(|s| !valid_name(s)) {
            errors.push(v.error(key, "expected 1-64 letters, digits, '.', '-' or '_'"));
            return None;
        }
        required(s, errors, v, key)
    };
    let voice_channel = snowflake(errors, v, "voice_channel");
    let text_channel = snowflake(errors, v, "text_channel");
    let speech_model = name(errors, v, "speech_model");
    let backend_model = name(errors, v, "backend_model");
    let summary_model = name(errors, v, "summary_model");
    let voice = name(errors, v, "voice");
    let backend_prices = prices(errors, v, "backend_prices");
    let summary_prices = prices(errors, v, "summary_prices");
    let search_price = number(
        errors,
        v,
        "search_price",
        valid_price,
        "expected a price in USD (0 or more)",
    );
    let search_price = required(search_price, errors, v, "search_price");
    let reasoning_effort = v
        .string(errors, "reasoning_effort")
        .map_or(defaults::REASONING_EFFORT, |e| e)
        .to_string();
    if !valid_effort(&reasoning_effort) {
        errors.push(v.error("reasoning_effort", "expected minimal, low, medium or high"));
    }
    let preferences = v.string(errors, "preferences").unwrap_or("").to_string();
    let zone = v
        .string(errors, "zone")
        .unwrap_or(defaults::ZONE)
        .to_string();
    if jiff::tz::TimeZone::get(&zone).is_err() {
        errors.push(v.error("zone", "unknown IANA time zone"));
    }
    let daily = v
        .integer(errors, "daily_minutes", 1, 24 * 60)
        .and_then(|m| u64::try_from(m).ok())
        .map_or(defaults::DAILY_CAP, Duration::from_mins);
    let total = v
        .integer(errors, "total_minutes", 0, i64::from(u32::MAX))
        .and_then(|m| u64::try_from(m).ok())
        .map_or(Duration::ZERO, Duration::from_mins);
    let spending = number(
        errors,
        v,
        "spending_cap_usd",
        |x| x.is_finite() && x > 0.0,
        "expected USD above 0",
    )
    .unwrap_or(defaults::SPENDING_CAP_USD);
    let idle = v
        .integer(errors, "idle_close_seconds", 1, 3600)
        .and_then(|s| u64::try_from(s).ok())
        .map_or(defaults::IDLE_CLOSE, Duration::from_secs);
    let wake = number(
        errors,
        v,
        "wake_dbfs",
        valid_wake,
        "expected dBFS strictly between -120 and 0",
    )
    .unwrap_or(defaults::WAKE_DBFS);
    let speech_price = number(
        errors,
        v,
        "speech_price_per_minute",
        valid_price,
        "expected USD per minute (0 or more)",
    );
    let transport = match v.string(errors, "transport") {
        None => Transport::WebSocket,
        Some(t) => Transport::parse(t).unwrap_or_else(|| {
            errors.push(v.error("transport", "expected websocket or webrtc"));
            Transport::WebSocket
        }),
    };
    let api_base = v
        .string(errors, "api_base")
        .unwrap_or(defaults::API_BASE)
        .to_string();
    if let Err(e) = cataly_voice::link::Endpoint::parse(&api_base) {
        errors.push(v.error("api_base", &e));
    }
    let key_file = v.string(errors, "key_file").map(PathBuf::from);
    if key_file.as_ref().is_some_and(|p| !p.is_absolute()) {
        errors.push(v.error("key_file", "must be an absolute path"));
    }
    let guild = discord
        .guild
        .and_then(|g| g.to_string().parse::<u64>().ok());
    let owner = discord
        .owner
        .and_then(|o| o.to_string().parse::<u64>().ok());
    if guild.is_none() {
        errors.push(ConfigError {
            path: "discord.guild".into(),
            message: "required when voice is enabled".into(),
        });
    }
    if owner.is_none() {
        errors.push(ConfigError {
            path: "discord.owner".into(),
            message: "required when voice is enabled".into(),
        });
    }
    v.used.push("enabled");
    let all = (
        (voice_channel, text_channel, guild, owner),
        (speech_model, backend_model, summary_model, voice),
        (backend_prices, summary_prices, search_price),
    );
    match all {
        (
            (Some(voice_channel), Some(text_channel), Some(guild), Some(owner)),
            (Some(speech_model), Some(backend_model), Some(summary_model), Some(voice)),
            (Some(backend_prices), Some(summary_prices), Some(search_price)),
        ) => Some(Box::new(VoiceOn {
            settings: Settings {
                guild,
                owner,
                voice_channel,
                text_channel,
                speech_model,
                backend_model,
                summary_model,
                voice,
                backend_prices,
                summary_prices,
                search_price,
                reasoning_effort,
                preferences,
                zone,
                daily_cap: daily,
                total_cap: total,
                spending_cap_usd: spending,
                idle_close: idle,
                wake_dbfs: wake,
                speech_price_per_minute: speech_price,
                transport,
                api_base,
            },
            key_file,
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice_of(text: &str) -> VoiceConfig {
        crate::config::parse(text).unwrap().voice
    }

    #[test]
    fn off_unless_enabled() {
        assert!(matches!(voice_of(""), VoiceConfig::Off));
        assert!(matches!(
            voice_of("[voice]\nspeech_model = 1"),
            VoiceConfig::Off
        ));
    }

    /// Check 1: voice stays off with a required setting missing and names the field.
    #[test]
    fn missing_fields_are_named() {
        let VoiceConfig::Refused(errors) = voice_of("[voice]\nenabled = true\n") else {
            panic!("refused expected");
        };
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        if cfg!(feature = "voice") {
            for field in [
                "voice.voice_channel",
                "voice.speech_model",
                "voice.backend_prices",
                "voice.search_price",
                "discord.guild",
            ] {
                assert!(paths.contains(&field), "{field} in {paths:?}");
            }
        } else {
            assert_eq!(paths, vec!["voice.enabled"]);
        }
    }

    #[cfg(feature = "voice")]
    #[test]
    fn full_settings_and_bad_values() {
        let base = r#"
[discord]
guild = "100000000000000001"
owner = "100000000000000002"
channels = ["100000000000000005"]
[voice]
enabled = true
voice_channel = "100000000000000003"
text_channel = "100000000000000004"
speech_model = "gpt-live-1"
backend_model = "gpt-5.6-luna"
summary_model = "gpt-6-luna"
voice = "marin"
search_price = 0.01
backend_prices = { input = 0.2, cached_input = 0.02, cache_write = 0.25, output = 1.2 }
summary_prices = { input = 0.1, cached_input = 0.01, cache_write = 0.125, output = 0.5 }
"#;
        let VoiceConfig::On(on) = voice_of(base) else {
            panic!("valid voice settings expected");
        };
        assert_eq!(on.settings.daily_cap, std::time::Duration::from_mins(30));
        assert!((on.settings.spending_cap_usd - 3.0).abs() < f64::EPSILON);
        let bad = format!("{base}wake_dbfs = 0\ntransport = \"carrier-pigeon\"\n");
        let VoiceConfig::Refused(errors) = voice_of(&bad) else {
            panic!("refused expected");
        };
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"voice.wake_dbfs") && paths.contains(&"voice.transport"));
    }
}
