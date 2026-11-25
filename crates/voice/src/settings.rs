//! Voice settings (VOICE-01, VOICE req. 49-50). `cataly`'s config module reads the owner's TOML into
//! this struct and checks each field with the functions here, so the rules live in one place.

use std::time::Duration;

/// Which transport carries the realtime session (owner decision 11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// JSON events with base64 PCM over one WebSocket.
    WebSocket,
    /// An SDP offer over HTTPS, then Opus over SRTP and a data channel for events.
    WebRtc,
}

impl Transport {
    /// Parses `websocket` or `webrtc`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "websocket" => Some(Self::WebSocket),
            "webrtc" => Some(Self::WebRtc),
            _ => None,
        }
    }

    /// The configured name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::WebSocket => "websocket",
            Self::WebRtc => "webrtc",
        }
    }
}

/// Per-million-token prices in USD.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Prices {
    /// Uncached input tokens.
    pub input: f64,
    /// Cached input tokens.
    pub cached_input: f64,
    /// Tokens written to the prompt cache.
    pub cache_write: f64,
    /// Output tokens.
    pub output: f64,
}

/// Validated voice settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The Discord guild (from the Discord settings).
    pub guild: u64,
    /// The owner's Discord user ID (from the Discord settings).
    pub owner: u64,
    /// The guild voice channel the bot joins.
    pub voice_channel: u64,
    /// The text channel for notes, notices and summaries.
    pub text_channel: u64,
    /// The realtime speech model.
    pub speech_model: String,
    /// The delegated backend model.
    pub backend_model: String,
    /// The end-of-session summary model.
    pub summary_model: String,
    /// The output voice.
    pub voice: String,
    /// Backend token prices.
    pub backend_prices: Prices,
    /// Summary token prices.
    pub summary_prices: Prices,
    /// USD per completed web search (may be 0).
    pub search_price: f64,
    /// Backend reasoning effort.
    pub reasoning_effort: String,
    /// The owner's conversation preferences, appended to the instructions.
    pub preferences: String,
    /// IANA zone name of the ledger's days; resolved by the host.
    pub zone: String,
    /// Speech per day.
    pub daily_cap: Duration,
    /// Speech over all days; zero means off.
    pub total_cap: Duration,
    /// Estimated backend spending per day in USD.
    pub spending_cap_usd: f64,
    /// Silence after which a session closes.
    pub idle_close: Duration,
    /// Owner level that counts as speech, in dBFS.
    pub wake_dbfs: f64,
    /// USD per minute of speech, for the summary's estimate only.
    pub speech_price_per_minute: Option<f64>,
    /// The realtime transport.
    pub transport: Transport,
    /// `https://api.openai.com`, or a loopback `http://` URL in tests.
    pub api_base: String,
}

/// Defaults of the optional settings (VOICE req. 50).
pub mod defaults {
    use std::time::Duration;
    /// Backend reasoning effort.
    pub const REASONING_EFFORT: &str = "low";
    /// The ledger's zone.
    pub const ZONE: &str = "UTC";
    /// Speech per day.
    pub const DAILY_CAP: Duration = Duration::from_mins(30);
    /// Estimated spending per day in USD.
    pub const SPENDING_CAP_USD: f64 = 3.0;
    /// Silence before a session closes.
    pub const IDLE_CLOSE: Duration = Duration::from_secs(120);
    /// Owner level that counts as speech.
    pub const WAKE_DBFS: f64 = -45.0;
    /// The provider.
    pub const API_BASE: &str = "https://api.openai.com";
}

/// The required fields, in the order their absence is reported.
pub const REQUIRED: [&str; 10] = [
    "voice_channel",
    "text_channel",
    "speech_model",
    "backend_model",
    "summary_model",
    "voice",
    "backend_prices",
    "summary_prices",
    "search_price",
    "key",
];

/// A price: finite and not negative.
#[must_use]
pub fn valid_price(p: f64) -> bool {
    p.is_finite() && p >= 0.0
}

/// The wake level: strictly between -120 and 0 dBFS.
#[must_use]
pub fn valid_wake(dbfs: f64) -> bool {
    dbfs.is_finite() && dbfs > -120.0 && dbfs < 0.0
}

/// Reasoning effort names the backend accepts.
#[must_use]
pub fn valid_effort(e: &str) -> bool {
    matches!(e, "minimal" | "low" | "medium" | "high")
}

/// Model and voice names: 1-64 of letters, digits, `.`, `-`, `_`.
#[must_use]
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}
