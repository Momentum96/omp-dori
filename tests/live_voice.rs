//! The owner-gated live voice test (owner decision 11, `DESIGN.md` 10.4): both transports against
//! the real realtime API.
//!
//! Ignored by default and never run in CI. It refuses to run unless the owner sets:
//!
//! - `OPENAI_API_KEY`: the provider key;
//! - `CATALY_LIVE_VOICE_MAX_USD`: the spending limit the owner approved, in USD;
//! - `CATALY_LIVE_VOICE_RECORDING`: a WAV file (24 kHz, mono, 16-bit) of the owner asking, for
//!   example, "Please pass this to the lead: check the build."
//!
//! Optional: `CATALY_LIVE_VOICE_TRANSPORTS` (`websocket`, `webrtc` or both comma-separated, the
//! default), `CATALY_LIVE_VOICE_API_BASE` (default `https://api.openai.com`), the model names
//! `CATALY_LIVE_VOICE_SPEECH_MODEL`, `_BACKEND_MODEL`, `_SUMMARY_MODEL`, `_VOICE`, and the prices
//! `CATALY_LIVE_VOICE_SPEECH_USD_PER_MIN`, `_BACKEND_USD_PER_MTOK` (input,cached,write,output) and
//! `_SUMMARY_USD_PER_MTOK`. Check the defaults against `OpenAI`'s current price list first.
//!
//! The test keeps its own ledger (backend and summary spending as recorded by voice, plus billed
//! speech seconds at the speech price) and checks it against the limit before every paid call; a
//! call over the limit is refused. For each transport it runs one spoken exchange from the
//! recording with one pass-to-lead whose receipt is proven; the call stays open while the backend
//! is continued with the tool output (`response.create`) and the lead's reply, queued meanwhile,
//! is offered as a reply piece (`session.commentary.append`) and proven spoken. Then a call that
//! stops at a deliberately low cap (`session.instructions.append`), and it prints the cost it used.
//!
//! Run: `cargo test --release --features voice --test live_voice -- --ignored --nocapture`

#![cfg(feature = "voice")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    // The in-memory host keeps the trait's async signatures without awaiting.
    clippy::unused_async_trait_impl
)]

use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use cataly_voice::{
    CallAudio, Day, Io, LedgerError, Passed, Posted, Prices, Reply, Settings, StatusPart,
    StatusRead, Transport, VoiceHost,
};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

const TEXT: u64 = 1;
/// The live test lead's reply to the request it receives.
const LEAD_REPLY: &str = "The build on main passed all checks and the release branch is ready.";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn prices(name: &str, default: Prices) -> Prices {
    let Some(v) = env(name) else { return default };
    let p: Vec<f64> = v.split(',').map(|x| x.trim().parse().unwrap()).collect();
    assert_eq!(p.len(), 4, "{name}: input,cached,write,output");
    Prices {
        input: p[0],
        cached_input: p[1],
        cache_write: p[2],
        output: p[3],
    }
}

static TLS: LazyLock<Arc<ClientConfig>> = LazyLock::new(|| {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
        ClientConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
});

/// The test's ledger and what voice did.
#[derive(Default)]
struct Book {
    day: Day,
    passes: Vec<(String, String)>,
    posts: Vec<String>,
    holds: Vec<String>,
    /// Realtime sessions started: WebSocket connections and WebRTC session POSTs.
    connects: usize,
    held: Option<String>,
    /// Reply pieces proven spoken: reply ID and byte offset.
    spoken: Vec<(i64, usize)>,
}

/// A host on the real network with an in-memory ledger checked before every paid call.
struct LiveHost {
    book: Mutex<Book>,
    max_usd: f64,
    speech_usd_per_min: f64,
}

impl LiveHost {
    fn spent(&self) -> f64 {
        let b = self.book.lock().unwrap();
        b.day.spend_usd + b.day.speech_seconds / 60.0 * self.speech_usd_per_min
    }

    /// Refuses a paid call once the approved limit is reached.
    fn budget(&self) -> Result<(), String> {
        let spent = self.spent();
        if spent >= self.max_usd {
            return Err(format!(
                "live budget reached: {spent:.4} of {:.4} USD",
                self.max_usd
            ));
        }
        Ok(())
    }

    async fn tls(host: &str, port: u16) -> Result<Box<dyn Io>, String> {
        let tcp = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect((host, port)))
            .await
            .map_err(|_| "connect timed out".to_string())?
            .map_err(|e| e.to_string())?;
        let name = ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
        let tls = tokio::time::timeout(
            Duration::from_secs(5),
            TlsConnector::from(Arc::clone(&TLS)).connect(name, tcp),
        )
        .await
        .map_err(|_| "TLS timed out".to_string())?
        .map_err(|e| e.to_string())?;
        Ok(Box::new(tls))
    }
}

impl VoiceHost for LiveHost {
    async fn connect(&self, tls: bool, host: &str, port: u16) -> Result<Box<dyn Io>, String> {
        assert!(tls, "the live test talks to the provider over TLS only");
        self.budget()?;
        self.book.lock().unwrap().connects += 1;
        Self::tls(host, port).await
    }

    async fn post_json(&self, url: &str, key: &str, body: Vec<u8>, deadline: Duration) -> Posted {
        if let Err(e) = self.budget() {
            return Posted::Failed(e);
        }
        let tail = url.strip_prefix("https://").unwrap();
        let (authority, path) = tail.split_at(tail.find('/').unwrap_or(tail.len()));
        if path == "/v1/live/sessions" {
            self.book.lock().unwrap().connects += 1;
        }
        let (host, port) = authority
            .rsplit_once(':')
            .map_or((authority, 443), |(h, p)| (h, p.parse().unwrap()));
        let work = async {
            let io = Self::tls(host, port).await?;
            let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
                .await
                .map_err(|e| e.to_string())?;
            tokio::spawn(conn);
            let req = hyper::Request::post(path)
                .header("host", authority)
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(body)))
                .map_err(|e| e.to_string())?;
            let resp = send.send_request(req).await.map_err(|e| e.to_string())?;
            let status = resp.status().as_u16();
            let bytes = resp
                .into_body()
                .collect()
                .await
                .map_err(|e| e.to_string())?
                .to_bytes()
                .to_vec();
            Ok::<_, String>((status, bytes))
        };
        match tokio::time::timeout(deadline, work).await {
            Err(_) => Posted::Uncertain("deadline".into()),
            Ok(Err(e)) => Posted::Uncertain(e),
            Ok(Ok((s, b))) if (200..300).contains(&s) => Posted::Done(b),
            Ok(Ok((s, b))) => Posted::Rejected(s, b),
        }
    }

    async fn voice_state(&self, _guild: u64, _channel: Option<u64>) -> Result<(), String> {
        Ok(())
    }

    async fn pass_to_lead(&self, request_id: &str, text: &str) -> Passed {
        // The live test's lead takes every request and proves its receipt at once; the hub's
        // receipt proof against OMP is covered by `tests/voice_hub.rs`.
        self.book
            .lock()
            .unwrap()
            .passes
            .push((request_id.into(), text.into()));
        Passed::Received
    }

    async fn status(&self, part: StatusPart) -> StatusRead {
        StatusRead {
            part,
            read_ms: 0,
            text: Ok("lane build: working, tests pass".into()),
        }
    }

    async fn post(&self, _channel: u64, text: &str) -> Result<(), String> {
        self.book.lock().unwrap().posts.push(text.into());
        Ok(())
    }

    async fn ledger(&self, _zone: &str) -> Result<(Day, f64), LedgerError> {
        let d = self.book.lock().unwrap().day.clone();
        let total = d.speech_seconds;
        Ok((d, total))
    }

    async fn record(&self, _zone: &str, s: f64, n: u32, usd: f64) -> Result<(), LedgerError> {
        let mut b = self.book.lock().unwrap();
        b.day.speech_seconds += s;
        b.day.sessions += n;
        b.day.spend_usd += usd;
        Ok(())
    }

    async fn held(&self) -> Result<Option<String>, LedgerError> {
        Ok(self.book.lock().unwrap().held.clone())
    }

    async fn hold(&self, reason: &str) -> Result<(), LedgerError> {
        let mut b = self.book.lock().unwrap();
        b.holds.push(reason.into());
        b.held = Some(reason.into());
        Ok(())
    }

    async fn replies(&self) -> Result<Vec<Reply>, LedgerError> {
        // The lead answers the first request once it is received.
        let b = self.book.lock().unwrap();
        let spoken = b.spoken.last().map_or(0, |s| s.1);
        Ok(b.passes
            .first()
            .filter(|_| spoken < LEAD_REPLY.len())
            .map(|(id, _)| Reply {
                id: 1,
                request_id: id.clone(),
                subject: "build".into(),
                text: LEAD_REPLY.into(),
                spoken,
            })
            .into_iter()
            .collect())
    }

    async fn spoken(&self, id: i64, spoken: usize) -> Result<(), LedgerError> {
        self.book.lock().unwrap().spoken.push((id, spoken));
        Ok(())
    }

    fn log(&self, message: &str) {
        eprintln!("voice: {message}");
    }

    async fn cap_notice_due(&self, _zone: &str) -> bool {
        true
    }
}

/// The owner's recording as 20 ms frames of 24 kHz mono.
fn recording(path: &str) -> Vec<Vec<i16>> {
    let bytes = std::fs::read(path).unwrap();
    let pcm = if bytes.starts_with(b"RIFF") {
        let at = bytes
            .windows(4)
            .position(|w| w == b"data")
            .expect("a WAV data chunk");
        &bytes[at + 8..]
    } else {
        &bytes[..]
    };
    let samples: Vec<i16> = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    samples
        .chunks(480)
        .map(|c| {
            let mut f = c.to_vec();
            f.resize(480, 0);
            f
        })
        .collect()
}

struct Gate {
    key: String,
    max_usd: f64,
    frames: Vec<Vec<i16>>,
    base: String,
    speech_usd_per_min: f64,
}

/// The owner's go-ahead, or why the test refuses.
fn gate() -> Gate {
    let (Some(key), Some(max), Some(rec)) = (
        env("OPENAI_API_KEY"),
        env("CATALY_LIVE_VOICE_MAX_USD"),
        env("CATALY_LIVE_VOICE_RECORDING"),
    ) else {
        panic!(
            "refused: set OPENAI_API_KEY, CATALY_LIVE_VOICE_MAX_USD (the approved limit) and \
             CATALY_LIVE_VOICE_RECORDING (24 kHz mono WAV) to run the live voice test"
        );
    };
    let max_usd: f64 = max.parse().expect("CATALY_LIVE_VOICE_MAX_USD in USD");
    assert!(max_usd > 0.0 && max_usd.is_finite(), "a positive limit");
    Gate {
        key,
        max_usd,
        frames: recording(&rec),
        base: env("CATALY_LIVE_VOICE_API_BASE").unwrap_or_else(|| "https://api.openai.com".into()),
        speech_usd_per_min: env("CATALY_LIVE_VOICE_SPEECH_USD_PER_MIN")
            .map_or(0.06, |v| v.parse().unwrap()),
    }
}

fn settings(g: &Gate, transport: Transport) -> Settings {
    let name = |var: &str, default: &str| env(var).unwrap_or_else(|| default.to_string());
    Settings {
        guild: 1,
        owner: 2,
        voice_channel: 3,
        text_channel: TEXT,
        speech_model: name("CATALY_LIVE_VOICE_SPEECH_MODEL", "gpt-live-1"),
        backend_model: name("CATALY_LIVE_VOICE_BACKEND_MODEL", "gpt-5.6-luna"),
        summary_model: name("CATALY_LIVE_VOICE_SUMMARY_MODEL", "gpt-6-luna"),
        voice: name("CATALY_LIVE_VOICE_VOICE", "marin"),
        backend_prices: prices(
            "CATALY_LIVE_VOICE_BACKEND_USD_PER_MTOK",
            Prices {
                input: 0.25,
                cached_input: 0.025,
                cache_write: 0.3,
                output: 2.0,
            },
        ),
        summary_prices: prices(
            "CATALY_LIVE_VOICE_SUMMARY_USD_PER_MTOK",
            Prices {
                input: 0.25,
                cached_input: 0.025,
                cache_write: 0.3,
                output: 2.0,
            },
        ),
        search_price: 0.01,
        reasoning_effort: "low".into(),
        preferences: String::new(),
        zone: "UTC".into(),
        daily_cap: Duration::from_mins(5),
        total_cap: Duration::ZERO,
        spending_cap_usd: g.max_usd,
        idle_close: Duration::from_secs(20),
        wake_dbfs: -45.0,
        speech_price_per_minute: Some(g.speech_usd_per_min),
        transport,
        api_base: g.base.clone(),
    }
}

struct Run {
    owner: mpsc::Sender<Vec<i16>>,
    played: Arc<Mutex<usize>>,
    presence: watch::Sender<bool>,
    control: mpsc::Sender<cataly_voice::Control>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

fn run(host: &Arc<LiveHost>, s: Settings, key: &str) -> Run {
    let (owner, owner_rx) = mpsc::channel(4096);
    let (play_tx, mut play_rx) = mpsc::channel::<Vec<i16>>(4096);
    let (presence, presence_rx) = watch::channel(false);
    let (control, control_rx) = mpsc::channel(8);
    let (stop, stop_rx) = watch::channel(false);
    let played = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&played);
    tokio::spawn(async move {
        while play_rx.recv().await.is_some() {
            *counter.lock().unwrap() += 1;
        }
    });
    let task = tokio::spawn(cataly_voice::run_with_audio(
        Arc::new(s),
        key.to_string(),
        Arc::clone(host),
        CallAudio {
            owner: owner_rx,
            play: play_tx,
        },
        presence_rx,
        control_rx,
        stop_rx,
    ));
    Run {
        owner,
        played,
        presence,
        control,
        stop,
        task,
    }
}

impl Run {
    async fn feed(&self, frames: &[Vec<i16>]) {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        // An interval's first tick is immediate: taking it here keeps repeated short feeds at real
        // time (else five frames go out in 80 ms, and a long quiet wait overflows input pacing).
        tick.tick().await;
        for f in frames {
            tick.tick().await;
            self.owner.send(f.clone()).await.unwrap();
        }
    }

    async fn quiet_until(&self, limit: Duration, cond: impl Fn() -> bool) -> bool {
        let end = Instant::now() + limit;
        let silence = vec![vec![0i16; 480]; 5];
        while Instant::now() < end {
            if cond() {
                return true;
            }
            self.feed(&silence).await;
        }
        cond()
    }

    async fn finish(self) {
        let _ = self.stop.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(90), self.task).await;
    }
}

async fn transport(g: &Gate, t: Transport) -> f64 {
    let host = Arc::new(LiveHost {
        book: Mutex::default(),
        max_usd: g.max_usd,
        speech_usd_per_min: g.speech_usd_per_min,
    });
    host.budget().unwrap();
    // One exchange from the recording, with one pass-to-lead and its proven receipt.
    let r = run(&host, settings(g, t), &g.key);
    r.presence.send(true).unwrap();
    r.feed(&vec![vec![0i16; 480]; 100]).await;
    r.feed(&g.frames).await;
    r.quiet_until(Duration::from_secs(60), || {
        let book = host.book.lock().unwrap();
        book.held.is_some() || (!book.passes.is_empty() && *r.played.lock().unwrap() > 0)
    })
    .await;
    let played = *r.played.lock().unwrap();
    // The lead's reply arrives while the delegation is continued; it is spoken once the backend
    // finished, so a rejected continuation or reply piece holds voice here.
    r.control
        .send(cataly_voice::Control::Replies)
        .await
        .unwrap();
    r.quiet_until(Duration::from_secs(60), || {
        let book = host.book.lock().unwrap();
        book.held.is_some() || book.spoken.contains(&(1, LEAD_REPLY.len()))
    })
    .await;
    let spoken = host
        .book
        .lock()
        .unwrap()
        .spoken
        .contains(&(1, LEAD_REPLY.len()));
    r.finish().await;
    let (passes, holds) = {
        let book = host.book.lock().unwrap();
        (book.passes.len(), book.holds.clone())
    };
    println!(
        "LIVE {t:?}: exchange: {passes} pass(es), {played} frames played, reply spoken {spoken}, \
         holds {holds:?}, cost so far {:.4} USD",
        host.spent()
    );
    assert!(
        passes > 0 && played > 0,
        "{t:?}: the model answered and passed the request ({played} frames)"
    );
    {
        let book = host.book.lock().unwrap();
        assert_eq!(book.passes.len(), 1, "{t:?}: sent exactly once");
        assert!(
            book.holds.is_empty(),
            "{t:?}: no provider failure: {:?}",
            book.holds
        );
    }
    assert!(
        spoken,
        "{t:?}: the lead's reply was spoken after the delegation"
    );

    // A deliberately low cap: the day is 8 s of speech short of it, so the greeting session winds
    // down at the cap and closes; the owner speaking again starts nothing and gets one notice.
    let mut s = settings(g, t);
    let used = host.book.lock().unwrap().day.speech_seconds;
    s.daily_cap = Duration::from_secs_f64(used + 8.0);
    host.budget().unwrap();
    let r = run(&host, s, &g.key);
    r.presence.send(true).unwrap();
    r.quiet_until(Duration::from_secs(25), || false).await;
    let connects = host.book.lock().unwrap().connects;
    r.feed(&g.frames).await;
    let notice = r
        .quiet_until(Duration::from_secs(10), || {
            host.book
                .lock()
                .unwrap()
                .posts
                .iter()
                .any(|p| p.starts_with("Voice is off for now"))
        })
        .await;
    r.finish().await;
    let (posts, holds) = {
        let book = host.book.lock().unwrap();
        (book.posts.clone(), book.holds.clone())
    };
    assert!(
        notice,
        "{t:?}: the cap notice went out; holds {holds:?}, posts {posts:?}, cost so far {:.4} USD",
        host.spent()
    );
    assert_eq!(
        host.book.lock().unwrap().connects,
        connects,
        "{t:?}: no new session once the cap is reached"
    );
    let spent = host.spent();
    println!(
        "LIVE {t:?}: cost used {spent:.4} USD (limit {:.4})",
        g.max_usd
    );
    spent
}

/// The transports to run, from `CATALY_LIVE_VOICE_TRANSPORTS`; both by default.
fn transports() -> Vec<Transport> {
    let Some(v) = env("CATALY_LIVE_VOICE_TRANSPORTS") else {
        return vec![Transport::WebSocket, Transport::WebRtc];
    };
    v.split(',')
        .map(|t| {
            Transport::parse(t.trim())
                .unwrap_or_else(|| panic!("CATALY_LIVE_VOICE_TRANSPORTS: unknown transport {t}"))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live: real realtime API, owner-approved spending limit"]
async fn live_voice_both_transports() {
    let mut g = gate();
    let mut total = 0.0;
    for t in transports() {
        assert!(g.max_usd > 0.0, "the limit is used up before {t:?}");
        let spent = transport(&g, t).await;
        g.max_usd -= spent;
        total += spent;
    }
    println!("LIVE total cost used {total:.4} USD");
}
