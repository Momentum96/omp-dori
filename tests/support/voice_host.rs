//! A fake `VoiceHost` and the test recording for voice tests.
//!
//! The "recorded audio" is a synthetic recording made here: a voiced sound (a 140 Hz pulse train
//! shaped by two formants) under a syllable envelope, at 24 kHz mono, with quiet room noise
//! between utterances. The build machine has no microphone or speech synthesiser; the signal has
//! the level, onset and pauses the call logic reacts to.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::missing_panics_doc,
    // Synthetic signal casts are bounded by construction; the fake's trait methods keep the
    // trait's async signatures without awaiting anything.
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::unused_async_trait_impl,
    clippy::similar_names,
    clippy::map_unwrap_or,
    clippy::chunks_exact_to_as_chunks
)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cataly_voice::{
    Day, GatewayEvent, Io, LedgerError, Passed, Posted, Reply, StatusPart, StatusRead, VoiceHost,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 20 ms of the recording: `loud` selects speech or room noise; `n` makes frames differ.
pub fn frame(loud: bool, n: usize) -> Vec<i16> {
    (0..480)
        .map(|i| {
            let t = (n * 480 + i) as f64 / 24_000.0;
            if loud {
                let syllable = 0.6 + 0.4 * (t * 2.0 * std::f64::consts::PI * 4.0).sin().abs();
                let voiced = (1..12)
                    .map(|h| {
                        let f = 140.0 * f64::from(h);
                        let formant = (-((f - 700.0) / 300.0).powi(2)).exp()
                            + 0.6 * (-((f - 1200.0) / 400.0).powi(2)).exp();
                        formant * (t * 2.0 * std::f64::consts::PI * f).sin()
                    })
                    .sum::<f64>();
                (voiced * 5000.0 * syllable) as i16
            } else {
                // Room noise around -66 dBFS, deterministic.
                let x = ((n * 480 + i) as u64).wrapping_mul(6_364_136_223_846_793_005) >> 58;
                (x as i16) - 32
            }
        })
        .collect()
}

/// Everything the fake host saw.
#[derive(Debug, Default)]
pub struct Seen {
    pub passes: Vec<(String, String)>,
    pub posts: Vec<(u64, String)>,
    pub holds: Vec<String>,
    pub records: Vec<(f64, u32, f64)>,
    pub spoken: Vec<(i64, usize)>,
    pub logs: Vec<String>,
    pub joins: Vec<Option<u64>>,
}

/// A fake host: loopback network, an in-memory ledger and reply queue, scripted lead outcomes.
pub struct FakeHost {
    pub seen: Mutex<Seen>,
    pub outcomes: Mutex<Vec<Passed>>,
    pub day: Mutex<Day>,
    pub corrupt: bool,
    pub held: Mutex<Option<String>>,
    pub replies: Mutex<BTreeMap<i64, Reply>>,
    pub gateway: Mutex<Option<tokio::sync::mpsc::Sender<GatewayEvent>>>,
}

impl FakeHost {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::default(),
            outcomes: Mutex::default(),
            day: Mutex::new(Day {
                day: "2026-11-25".into(),
                ..Day::default()
            }),
            corrupt: false,
            held: Mutex::default(),
            replies: Mutex::default(),
            gateway: Mutex::default(),
        })
    }
}

impl VoiceHost for FakeHost {
    async fn connect(&self, tls: bool, host: &str, port: u16) -> Result<Box<dyn Io>, String> {
        assert!(!tls, "tests stay on loopback");
        let s = TcpStream::connect((host, port))
            .await
            .map_err(|e| e.to_string())?;
        Ok(Box::new(s))
    }

    async fn post_json(&self, url: &str, key: &str, body: Vec<u8>, deadline: Duration) -> Posted {
        let rest = url.strip_prefix("http://").unwrap();
        let (authority, path) = rest.split_at(rest.find('/').unwrap());
        let work = async {
            let mut s = TcpStream::connect(authority)
                .await
                .map_err(|e| e.to_string())?;
            let head = format!(
                "POST {path} HTTP/1.1\r\nhost: {authority}\r\nauthorization: Bearer {key}\r\n\
                 content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            s.write_all(head.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            s.write_all(&body).await.map_err(|e| e.to_string())?;
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.map_err(|e| e.to_string())?;
            Ok::<_, String>(resp)
        };
        match tokio::time::timeout(deadline, work).await {
            Err(_) => Posted::Uncertain("timeout".into()),
            Ok(Err(e)) => Posted::Failed(e),
            Ok(Ok(resp)) => {
                let text = String::from_utf8_lossy(&resp).to_string();
                let status: u16 = text[9..12].parse().unwrap_or(0);
                let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
                if (200..300).contains(&status) {
                    Posted::Done(body.as_bytes().to_vec())
                } else {
                    Posted::Rejected(status, body.as_bytes().to_vec())
                }
            }
        }
    }

    async fn voice_state(&self, _guild: u64, channel: Option<u64>) -> Result<(), String> {
        self.seen.lock().unwrap().joins.push(channel);
        Ok(())
    }

    async fn pass_to_lead(&self, request_id: &str, text: &str) -> Passed {
        self.seen
            .lock()
            .unwrap()
            .passes
            .push((request_id.into(), text.into()));
        let mut o = self.outcomes.lock().unwrap();
        if o.is_empty() {
            Passed::Received
        } else {
            o.remove(0)
        }
    }

    async fn status(&self, part: StatusPart) -> StatusRead {
        StatusRead {
            part,
            read_ms: 1,
            text: Ok(match part {
                StatusPart::Lanes => "lane build: working since 10:00".into(),
                StatusPart::Reports => "build: tests pass".into(),
            }),
        }
    }

    async fn post(&self, channel: u64, text: &str) -> Result<(), String> {
        self.seen.lock().unwrap().posts.push((channel, text.into()));
        Ok(())
    }

    async fn ledger(&self, _zone: &str) -> Result<(Day, f64), LedgerError> {
        if self.corrupt {
            return Ok((
                Day {
                    speech_seconds: -1.0,
                    ..Day::default()
                },
                0.0,
            ));
        }
        let d = self.day.lock().unwrap().clone();
        let total = d.speech_seconds;
        Ok((d, total))
    }

    async fn record(&self, _zone: &str, s: f64, n: u32, usd: f64) -> Result<(), LedgerError> {
        self.seen.lock().unwrap().records.push((s, n, usd));
        let mut d = self.day.lock().unwrap();
        d.speech_seconds += s;
        d.sessions += n;
        d.spend_usd += usd;
        Ok(())
    }

    async fn held(&self) -> Result<Option<String>, LedgerError> {
        Ok(self.held.lock().unwrap().clone())
    }

    async fn hold(&self, reason: &str) -> Result<(), LedgerError> {
        self.seen.lock().unwrap().holds.push(reason.into());
        *self.held.lock().unwrap() = Some(reason.into());
        Ok(())
    }

    async fn replies(&self) -> Result<Vec<Reply>, LedgerError> {
        Ok(self.replies.lock().unwrap().values().cloned().collect())
    }

    async fn spoken(&self, id: i64, spoken: usize) -> Result<(), LedgerError> {
        self.seen.lock().unwrap().spoken.push((id, spoken));
        let mut r = self.replies.lock().unwrap();
        let done = r.get_mut(&id).is_some_and(|x| {
            x.spoken = spoken;
            spoken >= x.text.len()
        });
        if done {
            r.remove(&id);
        }
        Ok(())
    }

    fn log(&self, message: &str) {
        self.seen.lock().unwrap().logs.push(message.into());
    }

    async fn cap_notice_due(&self, _zone: &str) -> bool {
        true
    }
}
