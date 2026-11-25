//! The one HTTP client and policy (DESIGN 5.1; MSG-05, MSG-06; DISC-OUT req. 4-11): 5 s connect, a
//! per-attempt deadline, no redirects, response bodies capped at 2 MiB, TLS through rustls with the
//! ring provider and the webpki root set. Plain `http` is accepted only for a loopback host, which
//! is how tests point a relay at a fake server.
//!
//! Every call ends as one [`Outcome`]: done (2xx), rejected (4xx other than 429; nothing happened),
//! rate limited beyond what may be waited, or uncertain (network error after the request may have
//! left, timeout, 408, 5xx). Reads and idempotent writes retry with backoff 1 s, 2 s, 4 s, at most 4
//! attempts and 60 s (DISC-OUT req. 7-8); other writes are never repeated after an uncertain outcome.
//! A 429 asking for at most 60 s is waited out and retried for every kind, because nothing happened.
//! Once an attempt may have taken effect, the call ends done or uncertain, never rejected. Errors
//! never carry the URL or headers, so tokens cannot leak into logs.

use std::net::IpAddr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::Method;
use hyper::body::Bytes;
use hyper::header::{
    CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep, timeout};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest response body read.
pub(crate) const BODY_CAP: usize = 2 * 1024 * 1024;
/// Longest server-requested wait that is waited out.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
const AGENT: &str = concat!("cataly/", env!("CARGO_PKG_VERSION"));

/// Whether and how a request may be repeated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A read: retried on 429, 5xx and network errors.
    Read,
    /// A write that is safe to repeat (an enforced nonce, an absolute edit): retried like a read.
    Idempotent,
    /// A write repeated only after a 429 or a refused connection, never after an uncertain outcome.
    Once,
}

/// Deadlines and the retry budget of one call.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Policy {
    /// Deadline of one attempt, connect included.
    pub(crate) attempt: Duration,
    /// Attempts at most.
    pub(crate) attempts: u32,
    /// Retries stop once this much time has passed since the first attempt.
    pub(crate) budget: Duration,
}

impl Policy {
    /// Discord REST: 10 s per call, 4 attempts within 60 s (DISC-OUT req. 4, 7, 8).
    pub(crate) const RELAY: Self = Self {
        attempt: Duration::from_secs(10),
        attempts: 4,
        budget: Duration::from_secs(60),
    };
    /// Telegram and Slack: 15 s per call, reads 5 attempts waiting 1 s doubling (MSG req. 9-10).
    pub(crate) const CHAT: Self = Self {
        attempt: Duration::from_secs(15),
        attempts: 5,
        budget: Duration::from_secs(120),
    };
    /// One attempt of a long poll: its wait plus 10 s (MSG req. 9); the poll loop retries.
    pub(crate) const fn poll(wait: Duration) -> Self {
        Self {
            attempt: Duration::from_secs(wait.as_secs() + 10),
            attempts: 1,
            budget: Duration::ZERO,
        }
    }
    /// File transfers: 60 s, one attempt (MSG req. 9).
    pub(crate) const TRANSFER: Self = Self {
        attempt: Duration::from_secs(60),
        attempts: 1,
        budget: Duration::ZERO,
    };
}

/// A request. `path` starts with `/` and is joined to the base URL's path.
#[derive(Debug, Clone)]
pub(crate) struct Request<'a> {
    pub(crate) method: Method,
    pub(crate) path: &'a str,
    pub(crate) headers: Vec<(HeaderName, HeaderValue)>,
    pub(crate) json: Option<Vec<u8>>,
    pub(crate) kind: Kind,
}

/// A response of any status.
#[derive(Debug, Clone)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
}

impl Response {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// The server's requested delay: `Retry-After` in seconds, else a JSON `retry_after` at the top
    /// or under `parameters` (Telegram).
    pub(crate) fn retry_after(&self) -> Option<Duration> {
        self.header("retry-after")
            .and_then(|v| v.trim().parse::<f64>().ok())
            .or_else(|| {
                let v = serde_json::from_slice::<serde_json::Value>(&self.body).ok()?;
                v.get("retry_after")
                    .or_else(|| v.get("parameters")?.get("retry_after"))?
                    .as_f64()
            })
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map(Duration::from_secs_f64)
    }
}

/// How a call ended.
#[derive(Debug, Clone)]
pub(crate) enum Outcome {
    /// A 2xx answer.
    Done(Response),
    /// Nothing happened: a 3xx or a 4xx other than 429.
    Rejected(Response),
    /// The server asked to wait longer than may be waited, or the budget ran out while limited.
    RateLimited(Duration),
    /// The effect may or may not have happened.
    Uncertain(String),
    /// Nothing was sent (bad address, refused connection, TLS setup).
    Failed(String),
}

/// One attempt's failure.
#[derive(Debug)]
enum Attempt {
    /// The request never left: retry is safe for every kind.
    NotSent(String),
    /// The request may have reached the server.
    Unknown(String),
}

/// Where requests go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Base {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

impl Base {
    /// Parses `https://host[:port][/path]`, or `http://` for a loopback host only. User info, query
    /// and fragment are refused (DISC-OUT req. 10).
    pub(crate) fn parse(url: &str) -> Result<Self, String> {
        let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err("the URL must start with https:// (or http:// for loopback)".into());
        };
        if rest.contains(['@', '?', '#']) || rest.chars().any(char::is_whitespace) {
            return Err("the URL must not carry credentials, a query or a fragment".into());
        }
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, tail) = v6.split_once(']').ok_or("bad IPv6 host")?;
            (h.to_string(), tail.strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p)),
                None => (authority.to_string(), None),
            }
        };
        if host.is_empty() {
            return Err("the URL has no host".into());
        }
        let port = match port {
            Some(p) => p.parse::<u16>().map_err(|_| "bad port".to_string())?,
            None if tls => 443,
            None => 80,
        };
        let base = Self {
            tls,
            host,
            port,
            path: path.trim_end_matches('/').to_string(),
        };
        if !tls && !base.is_loopback() {
            return Err("plain http is allowed only for localhost or a loopback address".into());
        }
        Ok(base)
    }

    pub(crate) fn is_loopback(&self) -> bool {
        self.host.eq_ignore_ascii_case("localhost")
            || self.host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }

    pub(crate) fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match (self.tls, self.port) {
            (true, 443) | (false, 80) => host,
            (_, port) => format!("{host}:{port}"),
        }
    }
}

/// The client TLS setup, built once: ring provider, safe protocol defaults, webpki roots. `None` only
/// if the provider could not offer those defaults; then every TLS request fails unsent.
static TLS: LazyLock<Option<Arc<ClientConfig>>> = LazyLock::new(|| {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .ok()
        .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
});

/// A connected byte stream, plain (loopback only) or TLS.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Opens a TCP connection (5 s) and, for `tls`, a TLS session (5 s) under the shared client setup.
/// Plain connections are refused for a host that is not loopback (DISC-OUT req. 10).
pub(crate) async fn connect(tls: bool, host: &str, port: u16) -> Result<Box<dyn Io>, String> {
    let probe = Base {
        tls,
        host: host.to_string(),
        port,
        path: String::new(),
    };
    if !tls && !probe.is_loopback() {
        return Err("plain connections are allowed only to a loopback host".into());
    }
    let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| "connect timed out".to_string())?
        .map_err(|e| format!("connect failed: {}", e.kind()))?;
    let _ = tcp.set_nodelay(true);
    if !tls {
        return Ok(Box::new(tcp));
    }
    let name =
        ServerName::try_from(host.to_string()).map_err(|_| "invalid server name".to_string())?;
    let config = TLS
        .clone()
        .ok_or_else(|| "TLS setup unavailable".to_string())?;
    let stream = timeout(
        CONNECT_TIMEOUT,
        TlsConnector::from(config).connect(name, tcp),
    )
    .await
    .map_err(|_| "TLS handshake timed out".to_string())?
    .map_err(|e| format!("TLS handshake failed: {e}"))?;
    Ok(Box::new(stream))
}

/// A WebSocket client stream over [`connect`].
pub(crate) type WebSocket = tokio_tungstenite::WebSocketStream<Box<dyn Io>>;

/// Opens a WebSocket to `url` (`wss://`, or `ws://` for loopback) on `host:port`, frames capped at
/// `frame_max` bytes; connect and handshake are each bounded.
pub(crate) async fn websocket(
    tls: bool,
    host: &str,
    port: u16,
    url: &str,
    frame_max: usize,
) -> Result<WebSocket, String> {
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
    let io = connect(tls, host, port).await?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(frame_max))
        .max_frame_size(Some(frame_max));
    let (ws, _) = timeout(
        Duration::from_secs(10),
        tokio_tungstenite::client_async_with_config(url, io, Some(config)),
    )
    .await
    .map_err(|_| "WebSocket handshake timed out".to_string())?
    .map_err(|e| format!("WebSocket handshake failed: {e}"))?;
    Ok(ws)
}

/// Sends `req` to `base` under `policy`, calling `observe` with every response (rate-limit headers).
pub(crate) async fn call(
    base: &Base,
    req: &Request<'_>,
    policy: Policy,
    mut observe: impl FnMut(&Response),
) -> Outcome {
    let started = Instant::now();
    let mut backoff = Duration::from_secs(1);
    let mut attempt = 0;
    // Once an attempt may have reached the server, no later answer can prove nothing happened.
    let mut maybe_sent: Option<String> = None;
    loop {
        attempt += 1;
        let result = timeout(policy.attempt, once(base, req, BODY_CAP))
            .await
            .unwrap_or_else(|_| Err(Attempt::Unknown("no complete answer in time".into())));
        let elapsed = started.elapsed();
        let retry_allowed = |wait: Duration| {
            attempt < policy.attempts && elapsed.saturating_add(wait) <= policy.budget
        };
        let (wait, last) = match result {
            Ok(resp) if (200..300).contains(&resp.status) => {
                observe(&resp);
                return Outcome::Done(resp);
            }
            Ok(resp) if resp.status == 429 => {
                observe(&resp);
                let delay = resp.retry_after().unwrap_or(backoff);
                if delay > MAX_RETRY_AFTER || !retry_allowed(delay) {
                    return after_doubt(maybe_sent, Outcome::RateLimited(delay));
                }
                (delay, Outcome::RateLimited(delay))
            }
            Ok(resp) if resp.status == 408 || resp.status >= 500 => {
                observe(&resp);
                let what = format!("HTTP {}", resp.status);
                if req.kind == Kind::Once {
                    return Outcome::Uncertain(what);
                }
                maybe_sent = Some(what.clone());
                (backoff, Outcome::Uncertain(what))
            }
            Ok(resp) => {
                observe(&resp);
                return after_doubt(maybe_sent, Outcome::Rejected(resp));
            }
            Err(Attempt::NotSent(why)) => (backoff, Outcome::Failed(why)),
            Err(Attempt::Unknown(why)) => {
                if req.kind == Kind::Once {
                    return Outcome::Uncertain(why);
                }
                maybe_sent = Some(why.clone());
                (backoff, Outcome::Uncertain(why))
            }
        };
        if !retry_allowed(wait) {
            return after_doubt(maybe_sent, last);
        }
        sleep(wait).await;
        backoff = backoff.saturating_mul(2);
    }
}

/// A call whose earlier attempt may have taken effect ends uncertain unless it ended done.
fn after_doubt(maybe_sent: Option<String>, last: Outcome) -> Outcome {
    match (maybe_sent, last) {
        (_, done @ Outcome::Done(_)) => done,
        (Some(why), Outcome::Rejected(resp)) => {
            Outcome::Uncertain(format!("{why}, then HTTP {}", resp.status))
        }
        (Some(why), Outcome::RateLimited(d)) => {
            Outcome::Uncertain(format!("{why}, then rate limited for {} s", d.as_secs()))
        }
        (Some(why), Outcome::Failed(then)) => Outcome::Uncertain(format!("{why}, then {then}")),
        (_, last) => last,
    }
}

/// Most redirects a download follows.
const REDIRECTS_MAX: usize = 5;

/// Downloads `url` with GET, following at most 5 redirects, each to `https` (or `http` on a
/// loopback host, as for every request here), and reading at most `cap` bytes (IR req. 9). The
/// caller bounds the whole transfer. Errors never carry the URL.
pub(crate) async fn download(url: &str, cap: usize) -> Result<Vec<u8>, String> {
    let mut url = url.to_string();
    for _ in 0..=REDIRECTS_MAX {
        let (base, path) = split_url(&url)?;
        let req = Request {
            method: Method::GET,
            path: &path,
            headers: Vec::new(),
            json: None,
            kind: Kind::Read,
        };
        let resp = once(&base, &req, cap).await.map_err(|e| match e {
            Attempt::NotSent(why) | Attempt::Unknown(why) => why,
        })?;
        match resp.status {
            200..=299 => return Ok(resp.body),
            301 | 302 | 303 | 307 | 308 => {
                url = resp
                    .header("location")
                    .ok_or("a redirect named no location")?
                    .to_string();
            }
            status => return Err(format!("HTTP {status}")),
        }
    }
    Err("too many redirects".into())
}

/// Splits an absolute URL into its base (scheme and authority, checked by [`Base::parse`]) and the
/// path with its query. A fragment is refused.
fn split_url(url: &str) -> Result<(Base, String), String> {
    let scheme_end = url.find("://").map_or(0, |i| i + 3);
    let rest = url.get(scheme_end..).unwrap_or_default();
    let cut = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = url.split_at(scheme_end + cut);
    if path.contains('#') {
        return Err("the URL must not carry a fragment".into());
    }
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Ok((Base::parse(authority)?, path))
}

async fn once(base: &Base, req: &Request<'_>, cap: usize) -> Result<Response, Attempt> {
    let tcp = timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((base.host.as_str(), base.port)),
    )
    .await
    .map_err(|_| Attempt::NotSent("connect timed out".into()))?
    .map_err(|e| Attempt::NotSent(format!("connect failed: {}", e.kind())))?;
    if !base.tls {
        return exchange(tcp, base, req, cap).await;
    }
    let name = ServerName::try_from(base.host.clone())
        .map_err(|_| Attempt::NotSent("invalid server name".into()))?;
    let config = TLS
        .clone()
        .ok_or_else(|| Attempt::NotSent("TLS setup unavailable".into()))?;
    let tls = timeout(
        CONNECT_TIMEOUT,
        TlsConnector::from(config).connect(name, tcp),
    )
    .await
    .map_err(|_| Attempt::NotSent("TLS handshake timed out".into()))?
    .map_err(|e| Attempt::NotSent(format!("TLS handshake failed: {e}")))?;
    exchange(tls, base, req, cap).await
}

async fn exchange<S>(io: S, base: &Base, req: &Request<'_>, cap: usize) -> Result<Response, Attempt>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .map_err(|e| Attempt::NotSent(format!("HTTP handshake failed: {e}")))?;
    let driver = tokio::spawn(conn);
    let body = req.json.clone().unwrap_or_default();
    let mut builder = hyper::Request::builder()
        .method(req.method.clone())
        .uri(format!("{}{}", base.path, req.path))
        .header(HOST, base.authority())
        .header(USER_AGENT, AGENT)
        .header(CONTENT_LENGTH, body.len());
    if req.json.is_some() && !req.headers.iter().any(|(name, _)| *name == CONTENT_TYPE) {
        builder = builder.header(CONTENT_TYPE, "application/json");
    }
    for (name, value) in &req.headers {
        builder = builder.header(name, value);
    }
    let request = builder
        .body(Full::new(Bytes::from(body)))
        .map_err(|_| Attempt::NotSent("the request could not be built".into()))?;
    let result = async {
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| Attempt::Unknown(format!("request failed: {e}")))?;
        let status = response.status();
        let headers = response.headers().clone();
        let declared = headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok());
        if declared.is_some_and(|n| n > cap) {
            return Err(Attempt::Unknown(format!("declared body over {cap} bytes")));
        }
        let body = Limited::new(response.into_body(), cap)
            .collect()
            .await
            .map_err(|_| Attempt::Unknown(format!("response body unreadable or over {cap} bytes")))?
            .to_bytes()
            .to_vec();
        Ok(Response {
            status: status.as_u16(),
            headers,
            body,
        })
    }
    .await;
    driver.abort();
    result
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A loopback server answering each connection with the next canned response.
    pub(crate) async fn canned(responses: Vec<&'static str>) -> (Base, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&hits);
        tokio::spawn(async move {
            for raw in responses {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let _ = s.read(&mut buf).await;
                seen.fetch_add(1, Ordering::SeqCst);
                if raw.is_empty() {
                    continue; // drop the connection without an answer
                }
                let _ = s.write_all(raw.as_bytes()).await;
            }
        });
        (
            Base::parse(&format!("http://127.0.0.1:{port}/api")).unwrap(),
            hits,
        )
    }

    fn req(kind: Kind) -> Request<'static> {
        Request {
            method: Method::POST,
            path: "/x",
            headers: Vec::new(),
            json: Some(b"{}".to_vec()),
            kind,
        }
    }

    const FAST: Policy = Policy {
        attempt: Duration::from_secs(2),
        attempts: 4,
        budget: Duration::from_secs(10),
    };
    const OK: &str = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
    const LIMIT: &str = "HTTP/1.1 429 Too Many\r\nretry-after: 0.05\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    const FIVE: &str = "HTTP/1.1 502 Bad\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    const FOUR: &str =
        "HTTP/1.1 403 No\r\ncontent-length: 9\r\nconnection: close\r\n\r\n{\"a\":\"b\"}";

    #[test]
    fn base_urls_are_checked() {
        assert!(Base::parse("https://discord.com/api/v10").is_ok());
        assert!(Base::parse("http://127.0.0.1:9/x").unwrap().is_loopback());
        assert!(Base::parse("http://[::1]:9").is_ok());
        assert!(Base::parse("http://localhost:9").is_ok());
        for bad in [
            "http://example.com/api",
            "https://u:p@discord.com",
            "https://discord.com/?q=1",
            "https://discord.com/#f",
            "ftp://x",
            "https://:80",
        ] {
            assert!(Base::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(
            Base::parse("https://h:8443/a/").unwrap().authority(),
            "h:8443"
        );
    }

    #[tokio::test]
    async fn rate_limit_is_waited_out_and_writes_are_not_repeated_after_5xx() {
        let (base, hits) = canned(vec![LIMIT, OK]).await;
        assert!(matches!(
            call(&base, &req(Kind::Once), FAST, |_| {}).await,
            Outcome::Done(_)
        ));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        let (base, hits) = canned(vec![FIVE, OK]).await;
        assert!(matches!(
            call(&base, &req(Kind::Once), FAST, |_| {}).await,
            Outcome::Uncertain(_)
        ));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let (base, _) = canned(vec![FIVE, OK]).await;
        assert!(matches!(
            call(&base, &req(Kind::Idempotent), FAST, |_| {}).await,
            Outcome::Done(_)
        ));
        let (base, _) = canned(vec![FOUR]).await;
        match call(&base, &req(Kind::Read), FAST, |_| {}).await {
            Outcome::Rejected(r) => assert_eq!(r.status, 403),
            other => panic!("{other:?}"),
        }
        // A retried write whose first attempt may have landed is never reported as rejected.
        let (base, hits) = canned(vec![FIVE, FOUR]).await;
        assert!(matches!(
            call(&base, &req(Kind::Idempotent), FAST, |_| {}).await,
            Outcome::Uncertain(_)
        ));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn dropped_connection_is_uncertain_and_long_limits_are_reported() {
        let (base, _) = canned(vec![""]).await;
        assert!(matches!(
            call(&base, &req(Kind::Once), FAST, |_| {}).await,
            Outcome::Uncertain(_)
        ));
        let (base, _) = canned(vec![
            "HTTP/1.1 429 X\r\ncontent-length: 18\r\nconnection: close\r\n\r\n{\"retry_after\":90}",
        ])
        .await;
        match call(&base, &req(Kind::Once), FAST, |_| {}).await {
            Outcome::RateLimited(d) => assert_eq!(d, Duration::from_secs(90)),
            other => panic!("{other:?}"),
        }
        let closed = Base::parse("http://127.0.0.1:1").unwrap();
        let policy = Policy {
            attempts: 1,
            ..FAST
        };
        assert!(matches!(
            call(&closed, &req(Kind::Once), policy, |_| {}).await,
            Outcome::Failed(_)
        ));
    }
}
