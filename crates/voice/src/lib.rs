//! Cataly voice calls (`DESIGN.md` 5.2): the Discord voice connection with DAVE end-to-end
//! encryption, Opus, 24 kHz resampling and pacing, a realtime model session over WebSocket or WebRTC
//! behind one [`link::RealtimeLink`] seam, the usage ledger with hard caps, and the playback-proof
//! queue for lead replies.
//!
//! The crate reaches the hub only through [`VoiceHost`]; the hub implements it. Everything here is
//! compiled only in builds with the `voice` feature of `cataly`.

mod audio;
mod b64;
mod call;
mod discord;
mod host;
pub mod ledger;
pub mod link;
mod opus;
mod pace;
mod playback;
mod session;
pub mod settings;
mod text;
mod tools;

pub use call::{CallAudio, Control, Presence, VoiceStatus, run, run_with_audio};
pub use host::{
    GatewayEvent, Io, LedgerError, Passed, Posted, Reply, StatusPart, StatusRead, VoiceHost,
};
pub use ledger::{Caps, Day, Spend};
pub use settings::{Prices, Settings, Transport};
pub use text::{clean_note, clean_request};
pub use tools::{LANES_MAX, REPORT_MAX};

/// Discord audio: 48 kHz stereo, 20 ms frames of 960 samples per channel.
pub const DISCORD_RATE: u32 = 48_000;
/// Provider audio: 24 kHz mono 16-bit PCM.
pub const MODEL_RATE: u32 = 24_000;
/// Bytes of one second of provider audio.
pub const MODEL_BYTES_PER_SECOND: usize = 48_000;
