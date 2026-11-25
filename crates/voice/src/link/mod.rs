//! The realtime model link (VOICE-11, VOICE-12; owner decision 11). One session logic talks to the
//! provider through a [`Link`]: JSON events both ways, 24 kHz mono PCM in and out. The two
//! [`RealtimeLink`] transports differ only in how they start the session and carry those:
//!
//! - [`WebSocket`]: `session.start` as the first event; audio as base64 PCM inside JSON events.
//! - [`WebRtc`]: the session configuration and an SDP offer in one HTTPS POST; audio as Opus over
//!   SRTP; events over the `oai-events` data channel (str0m, sans-I/O).
//!
//! Config selects the transport; the other is never started.

mod rtc;
mod ws;

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::host::VoiceHost;
use crate::settings::Transport;

pub use rtc::WebRtc;
pub use ws::WebSocket;

/// Events and audio buffered towards the provider before the link counts as stuck.
const OUT_QUEUE: usize = 512;
/// Events and audio buffered from the provider.
const IN_QUEUE: usize = 1024;
/// Path of the GPT-Live session endpoint.
pub const SESSIONS_PATH: &str = "/v1/live/sessions";

/// What the provider sent.
#[derive(Debug, Clone)]
pub enum LinkEvent {
    /// A JSON event (other than output audio).
    Event(Value),
    /// Output audio, 24 kHz mono.
    Audio(Vec<i16>),
    /// An event that is not a JSON object with a `type`: a provider failure (VOICE req. 64).
    Malformed,
    /// The link ended.
    Closed(String),
}

/// What the session sends.
#[derive(Debug)]
pub(crate) enum Out {
    Event(Value),
    /// Input audio, 24 kHz mono 16-bit little-endian.
    Audio(Vec<u8>),
    /// Ends the transport at once.
    Close,
}

/// A running link.
pub struct Link {
    tx: mpsc::Sender<Out>,
    rx: mpsc::Receiver<LinkEvent>,
    task: JoinHandle<()>,
}

impl Link {
    fn new() -> (Self, mpsc::Receiver<Out>, mpsc::Sender<LinkEvent>) {
        let (tx, out_rx) = mpsc::channel(OUT_QUEUE);
        let (in_tx, rx) = mpsc::channel(IN_QUEUE);
        let link = Self {
            tx,
            rx,
            task: tokio::spawn(async {}),
        };
        (link, out_rx, in_tx)
    }

    /// Queues an event; false when the link is gone or stuck.
    pub(crate) fn send(&self, event: Value) -> bool {
        self.tx.try_send(Out::Event(event)).is_ok()
    }

    /// Queues input audio; false when the link is gone or stuck.
    pub(crate) fn audio(&self, pcm: Vec<u8>) -> bool {
        self.tx.try_send(Out::Audio(pcm)).is_ok()
    }

    /// The next provider event; `Closed` once the link ended.
    pub(crate) async fn recv(&mut self) -> LinkEvent {
        self.rx
            .recv()
            .await
            .unwrap_or_else(|| LinkEvent::Closed("link ended".into()))
    }

    /// Ends the transport now (the force-terminate of VOICE req. 64).
    pub(crate) fn terminate(&self) {
        let _ = self.tx.try_send(Out::Close);
        self.task.abort();
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Where the provider lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// TLS (`https`) or plain loopback (`http`).
    pub tls: bool,
    /// Host name.
    pub host: String,
    /// Port.
    pub port: u16,
}

impl Endpoint {
    /// `https://host[:port]`, or `http://` for a loopback host (tests).
    ///
    /// # Errors
    /// The base is not a scheme and host, or plain HTTP points beyond loopback.
    pub fn parse(base: &str) -> Result<Self, String> {
        let (tls, rest) = if let Some(r) = base.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = base.strip_prefix("http://") {
            (false, r)
        } else {
            return Err("api_base must start with https://".into());
        };
        let authority = rest.trim_end_matches('/');
        if authority.contains('/') || authority.is_empty() {
            return Err("api_base must be a scheme and host only".into());
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().map_err(|_| "invalid port".to_string())?),
            None => (authority, if tls { 443 } else { 80 }),
        };
        if !tls && !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
            return Err("plain http is allowed only for a loopback host".into());
        }
        Ok(Self {
            tls,
            host: host.to_string(),
            port,
        })
    }

    /// The base URL again, with `path` appended.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://{}:{}{path}", self.host, self.port)
    }
}

/// A transport that can start a realtime session.
pub trait RealtimeLink {
    /// Starts the session described by `session` (the `session` object of `session.start`) and
    /// returns the running link. The link reports `session.started` like any other event.
    fn open<H: VoiceHost>(
        host: &Arc<H>,
        endpoint: &Endpoint,
        key: &str,
        session: Value,
    ) -> impl Future<Output = Result<Link, String>> + Send;
}

/// Opens the configured transport.
pub(crate) async fn open<H: VoiceHost>(
    transport: Transport,
    host: &Arc<H>,
    endpoint: &Endpoint,
    key: &str,
    session: Value,
) -> Result<Link, String> {
    match transport {
        Transport::WebSocket => WebSocket::open(host, endpoint, key, session).await,
        Transport::WebRtc => WebRtc::open(host, endpoint, key, session).await,
    }
}

/// Classifies one incoming JSON text.
pub(crate) fn classify(text: &str) -> LinkEvent {
    match serde_json::from_str::<Value>(text) {
        Ok(v) if v.get("type").and_then(Value::as_str).is_some() => LinkEvent::Event(v),
        _ => LinkEvent::Malformed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        let e = Endpoint::parse("https://api.openai.com").unwrap();
        assert_eq!(
            (e.tls, e.host.as_str(), e.port),
            (true, "api.openai.com", 443)
        );
        assert_eq!(e.url("/v1/x"), "https://api.openai.com:443/v1/x");
        assert!(Endpoint::parse("http://example.com").is_err());
        assert_eq!(Endpoint::parse("http://127.0.0.1:9").unwrap().port, 9);
        assert!(matches!(classify("{}"), LinkEvent::Malformed));
        assert!(matches!(classify("nope"), LinkEvent::Malformed));
        assert!(matches!(classify(r#"{"type":"x"}"#), LinkEvent::Event(_)));
    }
}
