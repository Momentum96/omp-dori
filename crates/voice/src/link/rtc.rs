//! The WebRTC transport: an SDP offer and the session configuration in one HTTPS POST to
//! `/v1/live/sessions`, then Opus over SRTP and JSON events over the `oai-events` data channel,
//! driven by str0m (sans-I/O) on one UDP socket.
//!
//! The run loop keeps str0m's contract: every mutation is followed by draining `poll_output` until
//! it returns a timeout. Provider Opus (48 kHz) is decoded and brought down to 24 kHz mono with the
//! same converter Discord audio uses; input PCM goes up to 48 kHz stereo and out as 20 ms Opus
//! frames.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use str0m::change::SdpAnswer;
use str0m::channel::ChannelId;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::time::sleep_until;

use super::{Endpoint, Link, LinkEvent, Out, RealtimeLink, SESSIONS_PATH, classify};
use crate::audio::{FRAME_24K, Up, down, samples};
use crate::host::{Posted, VoiceHost};
use crate::opus::{Decode, Encode};

/// The data channel label the provider expects.
pub const EVENTS_LABEL: &str = "oai-events";
/// Deadline of the session-creating POST.
const POST: Duration = Duration::from_secs(20);
/// Events written before the data channel opens are kept, up to this many.
const EARLY_MAX: usize = 64;

/// The WebRTC transport.
pub struct WebRtc;

impl RealtimeLink for WebRtc {
    async fn open<H: VoiceHost>(
        host: &Arc<H>,
        endpoint: &Endpoint,
        key: &str,
        session: Value,
    ) -> Result<Link, String> {
        let local_ip = local_ip(endpoint).await;
        let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0))
            .await
            .map_err(|e| format!("UDP bind failed: {}", e.kind()))?;
        let local = socket
            .local_addr()
            .map_err(|e| format!("UDP address: {}", e.kind()))?;
        let mut rtc = Rtc::builder()
            .clear_codecs()
            .enable_opus(true, false)
            .build(Instant::now());
        let candidate = Candidate::host(local, "udp").map_err(|e| format!("ICE candidate: {e}"))?;
        rtc.add_local_candidate(candidate);
        let mut change = rtc.sdp_api();
        let mid = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let channel = change.add_channel(EVENTS_LABEL.to_string());
        let (offer, pending) = change
            .apply()
            .ok_or_else(|| "no SDP offer was produced".to_string())?;
        let body = json!({
            "session": session,
            "transport": {"type": "webrtc", "sdp": offer.to_sdp_string()},
        });
        let answer = match host
            .post_json(
                &endpoint.url(SESSIONS_PATH),
                key,
                body.to_string().into_bytes(),
                POST,
            )
            .await
        {
            Posted::Done(bytes) => serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| {
                    v.pointer("/transport/sdp")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .ok_or_else(|| "the session answer carries no SDP".to_string())?,
            Posted::Rejected(status, _) => return Err(format!("session refused ({status})")),
            Posted::Uncertain(e) => return Err(format!("session start uncertain: {e}")),
            Posted::Failed(e) => return Err(format!("session start failed: {e}")),
        };
        let answer = SdpAnswer::from_sdp_string(&answer)
            .map_err(|e| format!("unreadable SDP answer: {e}"))?;
        rtc.sdp_api()
            .accept_answer(pending, answer)
            .map_err(|e| format!("SDP answer refused: {e}"))?;
        let decode = Decode::new()?;
        let encode = Encode::new()?;
        let (mut link, out_rx, in_tx) = Link::new();
        let peer = Peer {
            rtc,
            socket,
            local,
            mid,
            channel,
            open: false,
            early: Vec::new(),
            decode,
            encode,
            up: Up::default(),
            input: Vec::new(),
            sent_samples: 0,
            in_tx,
        };
        link.task = tokio::spawn(peer.run(out_rx));
        Ok(link)
    }
}

/// The local address used to reach the provider: loopback for a loopback provider, else the
/// interface the system routes to it.
async fn local_ip(endpoint: &Endpoint) -> IpAddr {
    let fallback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    if !endpoint.tls {
        return fallback;
    }
    let Ok(probe) = UdpSocket::bind("0.0.0.0:0").await else {
        return fallback;
    };
    let target = format!("{}:{}", endpoint.host, endpoint.port);
    match tokio::time::timeout(Duration::from_secs(5), probe.connect(target)).await {
        Ok(Ok(())) => probe.local_addr().map_or(fallback, |a| a.ip()),
        _ => fallback,
    }
}

struct Peer {
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    mid: Mid,
    channel: ChannelId,
    open: bool,
    early: Vec<Vec<u8>>,
    decode: Decode,
    encode: Encode,
    up: Up,
    /// Input samples (24 kHz mono) not yet a full 20 ms frame.
    input: Vec<i16>,
    sent_samples: u64,
    in_tx: tokio::sync::mpsc::Sender<LinkEvent>,
}

impl Peer {
    async fn run(mut self, mut out_rx: tokio::sync::mpsc::Receiver<Out>) {
        let mut buf = vec![0u8; 2000];
        let reason = loop {
            let deadline = match self.drain().await {
                Ok(d) => d,
                Err(e) => break e,
            };
            tokio::select! {
                () = sleep_until(deadline.into()) => {
                    if let Err(e) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
                        break format!("WebRTC error: {e}");
                    }
                }
                got = self.socket.recv_from(&mut buf) => {
                    let Ok((n, source)) = got else { continue };
                    let Some(data) = buf.get(..n) else { continue };
                    let Ok(contents) = data.try_into() else { continue };
                    let input = Input::Receive(Instant::now(), Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: self.local,
                        contents,
                    });
                    if self.rtc.accepts(&input)
                        && let Err(e) = self.rtc.handle_input(input)
                    {
                        break format!("WebRTC error: {e}");
                    }
                }
                out = out_rx.recv() => match out {
                    None | Some(Out::Close) => break "closed by the session".into(),
                    Some(Out::Event(v)) => {
                        if let Err(e) = self.event(&v) {
                            break e;
                        }
                    }
                    Some(Out::Audio(pcm)) => {
                        if let Err(e) = self.audio(&pcm).await {
                            break e;
                        }
                    }
                },
            }
        };
        self.rtc.disconnect();
        let _ = self.drain().await;
        let _ = self.in_tx.send(LinkEvent::Closed(reason)).await;
    }

    /// Writes one event on the data channel, or keeps it until the channel opens.
    fn event(&mut self, v: &Value) -> Result<(), String> {
        let bytes = v.to_string().into_bytes();
        if !self.open {
            if self.early.len() >= EARLY_MAX {
                return Err("too many events before the data channel opened".into());
            }
            self.early.push(bytes);
            return Ok(());
        }
        let Some(mut ch) = self.rtc.channel(self.channel) else {
            return Err("the data channel is gone".into());
        };
        ch.write(false, &bytes)
            .map(|_| ())
            .map_err(|e| format!("data channel write: {e}"))
    }

    /// Sends input audio as 20 ms Opus frames; each frame is one mutation followed by a drain.
    async fn audio(&mut self, pcm: &[u8]) -> Result<(), String> {
        self.input.extend(samples(pcm));
        while self.input.len() >= FRAME_24K {
            let frame: Vec<i16> = self.input.drain(..FRAME_24K).collect();
            let stereo = self.up.push(&frame);
            let Some(packet) = self.encode.encode(&stereo) else {
                return Err("Opus encoding failed".into());
            };
            let Some(writer) = self.rtc.writer(self.mid) else {
                // Media is not negotiated yet; drop the frame like a lost packet.
                continue;
            };
            let Some(pt) = writer
                .payload_params()
                .next()
                .map(str0m::format::PayloadParams::pt)
            else {
                continue;
            };
            let time = MediaTime::new(self.sent_samples, Frequency::FORTY_EIGHT_KHZ);
            self.sent_samples += 960;
            writer
                .write(pt, Instant::now(), time, packet)
                .map_err(|e| format!("media write: {e}"))?;
            self.drain().await?;
        }
        Ok(())
    }

    /// Drains str0m until it asks for a timeout; returns that deadline.
    async fn drain(&mut self) -> Result<Instant, String> {
        loop {
            match self.rtc.poll_output() {
                Err(e) => return Err(format!("WebRTC error: {e}")),
                Ok(Output::Timeout(t)) => return Ok(t),
                Ok(Output::Transmit(t)) => {
                    let _ = self.socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Event(e)) => self.on_event(e).await?,
            }
        }
    }

    async fn on_event(&mut self, e: Event) -> Result<(), String> {
        match e {
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                return Err("ICE disconnected".into());
            }
            Event::ChannelOpen(id, _) if id == self.channel => {
                self.open = true;
                for bytes in std::mem::take(&mut self.early) {
                    if let Some(mut ch) = self.rtc.channel(self.channel) {
                        ch.write(false, &bytes)
                            .map_err(|e| format!("data channel write: {e}"))?;
                    }
                }
            }
            Event::ChannelClose(id) if id == self.channel => {
                return Err("the provider closed the data channel".into());
            }
            Event::ChannelData(d) if d.id == self.channel => {
                let event = std::str::from_utf8(&d.data).map_or(LinkEvent::Malformed, classify);
                self.in_tx
                    .send(event)
                    .await
                    .map_err(|_| "session gone".to_string())?;
            }
            Event::MediaData(m) => {
                if let Some(stereo) = self.decode.decode(&m.data) {
                    self.in_tx
                        .send(LinkEvent::Audio(down(&stereo)))
                        .await
                        .map_err(|_| "session gone".to_string())?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
