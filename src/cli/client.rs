//! The CLI side of the control socket: one connection, one request, bounded waits.

use std::fmt;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::proto::{self, ErrorBody, MAX_LINE, Request, Response};

/// Connecting must succeed within this time.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// A call failed.
#[derive(Debug)]
pub(crate) enum ClientError {
    /// Nothing answers on the socket.
    NoHub,
    Io(std::io::Error),
    Protocol(String),
    Remote(ErrorBody),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHub => f.write_str("the hub is not running"),
            Self::Io(e) => write!(f, "control socket: {e}"),
            Self::Protocol(e) => write!(f, "control socket: {e}"),
            Self::Remote(body) => f.write_str(&body.message),
        }
    }
}

pub(crate) struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl Client {
    pub(crate) fn connect(path: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path).map_err(|e| match e.kind() {
            ErrorKind::NotFound | ErrorKind::ConnectionRefused => ClientError::NoHub,
            _ => ClientError::Io(e),
        })?;
        stream
            .set_write_timeout(Some(CONNECT_TIMEOUT))
            .map_err(ClientError::Io)?;
        let writer = stream.try_clone().map_err(ClientError::Io)?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            next_id: 1,
        })
    }

    fn send(&mut self, method: &str, params: Value) -> Result<u64, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request {
            v: proto::VERSION,
            id,
            method,
            params,
        };
        let mut line =
            serde_json::to_string(&request).map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .map_err(ClientError::Io)?;
        Ok(id)
    }

    /// Reads one response line; `None` at end of stream. `timeout: None` waits without a bound (only
    /// for following events, which the owner ends).
    fn receive(&mut self, timeout: Option<Duration>) -> Result<Option<Response>, ClientError> {
        self.reader
            .get_ref()
            .set_read_timeout(timeout)
            .map_err(ClientError::Io)?;
        let mut line = Vec::new();
        let limit = u64::try_from(MAX_LINE).unwrap_or(u64::MAX) + 1;
        let read = (&mut self.reader)
            .take(limit)
            .read_until(b'\n', &mut line)
            .map_err(|e| match e.kind() {
                ErrorKind::WouldBlock | ErrorKind::TimedOut => {
                    ClientError::Protocol("no answer from the hub in time".into())
                }
                _ => ClientError::Io(e),
            })?;
        if read == 0 {
            return Ok(None);
        }
        let response: Response = serde_json::from_slice(&line)
            .map_err(|e| ClientError::Protocol(format!("bad reply: {e}")))?;
        if response.v != proto::VERSION {
            return Err(ClientError::Protocol(format!(
                "the hub speaks control protocol v{} and this cataly v{}; restart the hub",
                response.v,
                proto::VERSION
            )));
        }
        Ok(Some(response))
    }

    /// One request, one reply.
    pub(crate) fn call(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        let id = self.send(method, params)?;
        loop {
            let response = self
                .receive(Some(timeout))?
                .ok_or_else(|| ClientError::Protocol("the hub closed the connection".into()))?;
            if response.id != id && response.id != 0 {
                continue;
            }
            if let Some(err) = response.err {
                return Err(ClientError::Remote(err));
            }
            return Ok(response.ok.unwrap_or(Value::Null));
        }
    }

    /// Streams items of `method` to `each` until the hub closes the stream.
    pub(crate) fn follow(
        &mut self,
        method: &str,
        params: Value,
        mut each: impl FnMut(Value),
    ) -> Result<(), ClientError> {
        self.send(method, params)?;
        while let Some(response) = self.receive(None)? {
            if let Some(err) = response.err {
                return Err(ClientError::Remote(err));
            }
            if let Some(item) = response.item {
                each(item);
            }
        }
        Ok(())
    }
}
