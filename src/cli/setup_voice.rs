//! The voice step of `cataly setup`: `--voice on` turns the Discord voice assistant on and fills in
//! every `[voice]` key the file does not set yet; `--voice off` turns it off and keeps the rest.
//!
//! The defaults: the voice channel named by `--voice-channel`, else the server's only voice
//! channel, else the one named "General" or "일반", else a `Cataly` voice channel found or created;
//! the text channel is the lead's home channel; the models, voice and prices are the voice
//! crate's starting values; the zone is the system's; the caps keep voice's defaults unless
//! `--voice-daily-minutes` or `--voice-cap-usd` says otherwise. The provider key is the file named
//! by `--voice-key-file`, else `secrets/voice-key`. Cataly does not look for a key stored by OMP:
//! OMP keeps provider keys in its own credential store, and no public OMP command names a key file
//! without reading the key itself. With no key file the run stops before it enables anything.
//!
//! With voice on, a later run changes only what a voice flag names.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use super::args::Parsed;
use super::install::block_on;
use super::setup::{Item, in_file, set_rendered, switch, toml_string};
use super::setup_discord::Discord;
use super::{Failure, setup_discord};
use crate::config;
use crate::relay::discord::Snowflake;
use crate::sys::paths::Paths;

/// The flags that change one voice setting while voice is on.
const TUNING: [&str; 4] = [
    "--voice-channel",
    "--voice-daily-minutes",
    "--voice-cap-usd",
    "--voice-key-file",
];
/// The voice channel setup creates when the server has no obvious one.
const CHANNEL: &str = "Cataly";
/// Names Discord gives a new server's voice channel.
const GENERAL: [&str; 2] = ["General", "일반"];
const SECTION: &str = "voice";

/// What is left for after the owner's yes: a voice channel to create and the text channel, which
/// is the lead's home channel once the Discord channels are settled.
pub(super) struct Pending {
    create: Option<String>,
    text_channel: bool,
}

/// The voice settings the flags ask for, written into `text`, with the summary's lines.
pub(super) fn decide(
    p: &Parsed,
    paths: &Paths,
    planned: &config::Config,
    mut text: String,
    discord: Option<&Discord>,
    items: &mut Vec<Item>,
) -> Result<(String, Option<Pending>), Failure> {
    let flag = switch(p, "--voice")?;
    let tuned: Vec<&str> = TUNING
        .iter()
        .copied()
        .filter(|f| p.value(f).is_some())
        .collect();
    let on = enabled(&text);
    if let Some(first) = tuned.first()
        && (flag == Some(false) || (flag.is_none() && !on))
    {
        return Err(Failure::Usage(format!(
            "{first}: voice is off; add --voice on"
        )));
    }
    match flag {
        Some(false) => {
            if on {
                text = set_rendered(&text, SECTION, "enabled", "false");
            }
            items.push(Item::new(
                "voice",
                "off (the other [voice] keys are kept)",
                on,
            ));
            Ok((text, None))
        }
        Some(true) => turn_on(p, paths, planned, text, discord, items, true),
        None if !tuned.is_empty() => turn_on(p, paths, planned, text, discord, items, false),
        None => {
            if on {
                describe(&text, planned, None, items, false, false);
            } else {
                items.push(Item::new(
                    "voice",
                    "off: `--voice on` adds Discord voice calls, paid on your OpenAI key",
                    false,
                ));
            }
            Ok((text, None))
        }
    }
}

/// `--voice on` (`fill`), or a voice flag while voice is on.
fn turn_on(
    p: &Parsed,
    paths: &Paths,
    planned: &config::Config,
    mut text: String,
    discord: Option<&Discord>,
    items: &mut Vec<Item>,
    fill: bool,
) -> Result<(String, Option<Pending>), Failure> {
    let starting = starting()?;
    let Some(d) = discord else {
        return Err(Failure::Refused(
            "voice calls take place in Discord; set Discord up first (`cataly setup`)".into(),
        ));
    };
    let before = text.clone();
    let key = key_file(p, paths, &text, fill)?;
    if fill && !enabled(&text) {
        text = set_rendered(&text, SECTION, "enabled", "true");
    }
    let create = match channel(p, &text, d, fill)? {
        Channel::Keep => None,
        Channel::Found(id) => {
            text = set_rendered(
                &text,
                SECTION,
                "voice_channel",
                &toml_string(&id.to_string()),
            );
            None
        }
        Channel::Create(name) => Some(name),
    };
    if fill {
        for (k, v) in starting {
            if !in_file(&text, SECTION, k) {
                text = set_rendered(&text, SECTION, k, &v);
            }
        }
        if !in_file(&text, SECTION, "zone") {
            text = set_rendered(&text, SECTION, "zone", &toml_string(&zone()));
        }
    }
    // The caps the flags name replace whatever the file says.
    if let Some(v) = p.value("--voice-daily-minutes") {
        let minutes = v
            .parse::<u32>()
            .ok()
            .filter(|m| (1..=24 * 60).contains(m))
            .ok_or_else(|| Failure::Usage("--voice-daily-minutes: expected 1-1440".into()))?;
        text = set_rendered(&text, SECTION, "daily_minutes", &minutes.to_string());
    }
    if let Some(v) = p.value("--voice-cap-usd") {
        let usd = v
            .parse::<f64>()
            .ok()
            .filter(|u| u.is_finite() && *u > 0.0)
            .ok_or_else(|| Failure::Usage("--voice-cap-usd: expected USD above 0".into()))?;
        text = set_rendered(&text, SECTION, "spending_cap_usd", &format!("{usd:?}"));
    }
    if let Some(path) = key {
        text = set_rendered(
            &text,
            SECTION,
            "key_file",
            &toml_string(&path.display().to_string()),
        );
    }
    let text_channel = !in_file(&text, SECTION, "text_channel");
    let fills = text != before || create.is_some() || text_channel;
    let cost = |t: &str| {
        let v = table(t);
        ["daily_minutes", "spending_cap_usd", "key_file"].map(|k| v.get(k).cloned())
    };
    let cost_change = cost(&before) != cost(&text);
    describe(&text, planned, create.as_deref(), items, fills, cost_change);
    Ok((
        text,
        Some(Pending {
            create,
            text_channel,
        }),
    ))
}

/// The two summary lines of voice that is on.
fn describe(
    text: &str,
    planned: &config::Config,
    create: Option<&str>,
    items: &mut Vec<Item>,
    change: bool,
    cost_change: bool,
) {
    let v = table(text);
    let shown = |key: &str| match v.get(key) {
        Some(toml::Value::String(s)) => s.clone(),
        Some(toml::Value::Integer(i)) => i.to_string(),
        Some(toml::Value::Float(f)) => format!("{f:?}"),
        Some(_) => "?".to_string(),
        None => "the default".to_string(),
    };
    let channel = create.map_or_else(
        || format!("voice channel {}", shown("voice_channel")),
        |name| format!("voice channel {name} (to create)"),
    );
    let home = if v.contains_key("text_channel") {
        format!("#{}", shown("text_channel"))
    } else {
        planned
            .discord
            .home()
            .map_or_else(|| format!("#{}", setup_discord::LEAD), |h| format!("#{h}"))
    };
    items.push(Item::new(
        "voice",
        format!(
            "on: {channel}, its text in {home}; models {} / {} / {}, voice {}",
            shown("speech_model"),
            shown("backend_model"),
            shown("summary_model"),
            shown("voice")
        ),
        change,
    ));
    let minutes = shown("daily_minutes");
    let usd = shown("spending_cap_usd");
    let key = v
        .get("key_file")
        .and_then(toml::Value::as_str)
        .unwrap_or("secrets/voice-key");
    items.push(Item::new(
        "voice cost",
        format!(
            "calls cost money on your OpenAI key ({key}); capped at {minutes} minutes and {usd} USD \
             a day; check the prices in [voice] against OpenAI's price list"
        ),
        cost_change,
    ));
}

/// After the owner's yes and once the Discord channels are settled: creates the voice channel if
/// one is to be made, sets the text channel to the lead's home channel, and checks that voice
/// would start with what is written.
pub(super) fn apply(pending: &Pending, d: &Discord, mut text: String) -> Result<String, String> {
    if let Some(name) = &pending.create {
        let id = block_on(d.create_voice(name))?;
        text = set_rendered(
            &text,
            SECTION,
            "voice_channel",
            &toml_string(&id.to_string()),
        );
    }
    let config = config::parse(&text).map_err(|e| config::describe(&e))?;
    if pending.text_channel {
        let home = config
            .discord
            .home()
            .ok_or("the lead has no home channel for the call's text; run setup again")?;
        text = set_rendered(
            &text,
            SECTION,
            "text_channel",
            &toml_string(&home.to_string()),
        );
    }
    let config = config::parse(&text).map_err(|e| config::describe(&e))?;
    if let config::voice::VoiceConfig::Refused(errors) = &config.voice {
        return Err(format!(
            "voice would not start ({}); nothing was enabled",
            config::describe(errors)
        ));
    }
    Ok(text)
}

/// The voice channel to use.
enum Channel {
    /// The file names one and no flag asks for another.
    Keep,
    Found(Snowflake),
    Create(String),
}

fn channel(p: &Parsed, text: &str, d: &Discord, fill: bool) -> Result<Channel, Failure> {
    let flag = p.value("--voice-channel");
    if flag.is_none() && (!fill || in_file(text, SECTION, "voice_channel")) {
        return Ok(Channel::Keep);
    }
    let all = block_on(d.voice_channels()).map_err(Failure::Refused)?;
    let named = |name: &str| -> Vec<Snowflake> {
        let exact: Vec<Snowflake> = all
            .iter()
            .filter(|(_, n)| n == name)
            .map(|(id, _)| *id)
            .collect();
        if exact.is_empty() {
            all.iter()
                .filter(|(_, n)| n.to_lowercase() == name.to_lowercase())
                .map(|(id, _)| *id)
                .collect()
        } else {
            exact
        }
    };
    let several = |name: &str, n: usize| {
        Failure::Refused(format!(
            "{n} voice channels are named {name}; name one with --voice-channel <id>"
        ))
    };
    if let Some(wanted) = flag {
        if let Some(id) = Snowflake::parse(wanted) {
            return if all.iter().any(|(c, _)| *c == id) {
                Ok(Channel::Found(id))
            } else {
                Err(Failure::Refused(format!(
                    "--voice-channel: {} has no voice channel {id}",
                    d.server
                )))
            };
        }
        return match named(wanted).as_slice() {
            [] => Ok(Channel::Create(wanted.to_string())),
            [one] => Ok(Channel::Found(*one)),
            more => Err(several(wanted, more.len())),
        };
    }
    if let [(only, _)] = all.as_slice() {
        return Ok(Channel::Found(*only));
    }
    let general: Vec<Snowflake> = GENERAL.iter().flat_map(|g| named(g)).collect();
    if let [one] = general.as_slice() {
        return Ok(Channel::Found(*one));
    }
    match named(CHANNEL).as_slice() {
        [] => Ok(Channel::Create(CHANNEL.to_string())),
        [one] => Ok(Channel::Found(*one)),
        more => Err(several(CHANNEL, more.len())),
    }
}

/// The key file to write, if one is to be written. Turning voice on needs the file to exist,
/// owner-only; without one the run stops here, before anything is enabled.
fn key_file(p: &Parsed, paths: &Paths, text: &str, fill: bool) -> Result<Option<PathBuf>, Failure> {
    if let Some(flag) = p.value("--voice-key-file") {
        let path = std::path::absolute(flag)
            .map_err(|e| Failure::Usage(format!("--voice-key-file: {e}")))?;
        check_key(&path)?;
        return Ok(Some(path));
    }
    if !fill {
        return Ok(None);
    }
    if let Some(set) = table(text).get("key_file").and_then(toml::Value::as_str) {
        check_key(Path::new(set))?;
        return Ok(None);
    }
    let default = paths
        .config_file
        .parent()
        .unwrap_or(Path::new("/"))
        .join("secrets/voice-key");
    check_key(&default)?;
    Ok(Some(default))
}

fn check_key(path: &Path) -> Result<(), Failure> {
    let shown = path.display();
    match std::fs::metadata(path) {
        Ok(m) if !m.is_file() => Err(Failure::Refused(format!(
            "{shown} is not a file; save your OpenAI API key there, then run setup again"
        ))),
        Ok(m) if m.mode() & 0o077 != 0 => Err(Failure::Refused(format!(
            "{shown} is open to others; run `chmod 600 {shown}`, then run setup again"
        ))),
        Ok(_) => Ok(()),
        Err(_) => Err(Failure::Refused(format!(
            "no voice key: save your OpenAI API key in {shown} (`chmod 600 {shown}`), or name \
             another file with --voice-key-file <path>, then run `cataly setup --voice on` again; \
             voice was not enabled"
        ))),
    }
}

/// Whether the file turns voice on.
fn enabled(text: &str) -> bool {
    table(text)
        .get("enabled")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false)
}

/// The `[voice]` table of the file.
fn table(text: &str) -> toml::Table {
    toml::from_str::<toml::Table>(text)
        .ok()
        .and_then(|t| t.get(SECTION)?.as_table().cloned())
        .unwrap_or_default()
}

/// The system's IANA time zone, else UTC.
fn zone() -> String {
    let system = jiff::tz::TimeZone::system();
    system
        .iana_name()
        .filter(|z| jiff::tz::TimeZone::get(z).is_ok())
        .unwrap_or("UTC")
        .to_string()
}

/// The starting models, voice, prices and caps, rendered as TOML.
#[cfg(feature = "voice")]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the same signature as the build without voice, which refuses"
)]
fn starting() -> Result<Vec<(&'static str, String)>, Failure> {
    use cataly_voice::Prices;
    use cataly_voice::settings::{defaults, starting as s};
    let prices = |p: Prices| {
        format!(
            "{{ input = {:?}, cached_input = {:?}, cache_write = {:?}, output = {:?} }}",
            p.input, p.cached_input, p.cache_write, p.output
        )
    };
    Ok(vec![
        ("speech_model", toml_string(s::SPEECH_MODEL)),
        ("backend_model", toml_string(s::BACKEND_MODEL)),
        ("summary_model", toml_string(s::SUMMARY_MODEL)),
        ("voice", toml_string(s::VOICE)),
        ("backend_prices", prices(s::BACKEND_PRICES)),
        ("summary_prices", prices(s::SUMMARY_PRICES)),
        ("search_price", format!("{:?}", s::SEARCH_PRICE)),
        (
            "speech_price_per_minute",
            format!("{:?}", s::SPEECH_PRICE_PER_MINUTE),
        ),
        (
            "daily_minutes",
            (defaults::DAILY_CAP.as_secs() / 60).to_string(),
        ),
        (
            "spending_cap_usd",
            format!("{:?}", defaults::SPENDING_CAP_USD),
        ),
    ])
}

#[cfg(not(feature = "voice"))]
fn starting() -> Result<Vec<(&'static str, String)>, Failure> {
    Err(Failure::Refused(
        "this cataly is built without voice; install a release with voice, then \
         `cataly setup --voice on`"
            .into(),
    ))
}
