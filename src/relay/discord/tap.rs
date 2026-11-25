//! What the one Gateway connection shares with voice (VOICE-03, VOICE req. 51): the extra intents,
//! the dispatches voice reads and the frames voice sends (op 4). The relay knows no voice types; the
//! hub's voice task supplies the callbacks, so a build without voice carries none of it.

use serde_json::Value;

use super::gateway::Ops;
use crate::config::Config;

/// Dispatches handed to the tap.
pub(crate) const DISPATCHES: [&str; 3] =
    ["GUILD_CREATE", "VOICE_STATE_UPDATE", "VOICE_SERVER_UPDATE"];

/// A Gateway fact for the tap. Only a voice build reads them.
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
pub(crate) enum Seen<'a> {
    /// READY or RESUMED: the bot's user ID.
    Up(u64),
    /// One of [`DISPATCHES`].
    Dispatch(&'a str, &'a Value),
    /// The connection went down.
    Down,
}

/// The tap on the Gateway connection.
pub(crate) struct Tap {
    /// Whether the config wants the tap: voice intents are requested only then.
    pub(crate) wanted: Box<dyn Fn(&Config) -> bool + Send + Sync>,
    pub(crate) seen: Box<dyn Fn(Seen<'_>) + Send + Sync>,
    /// Frames to send once the connection is established.
    pub(crate) ops: Ops,
}
