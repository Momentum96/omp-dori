//! The CLI side of the control socket: one connection, one request, bounded waits.

use std::fmt;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::proto::{self, ErrorBody, MAX_LINE, Request, Response};
use crate::sys::fs::FsError;

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
    /// The socket's directory is not the owner's private one; nothing was sent.
    Unsafe(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHub => f.write_str("the hub is not running"),
            Self::Io(e) => write!(f, "control socket: {e}"),
            Self::Protocol(e) => write!(f, "control socket: {e}"),
            Self::Remote(body) => f.write_str(&body.message),
            Self::Unsafe(e) => write!(f, "control socket not used: {e}"),
        }
    }
}

/// Refuses a socket whose directory is not the owner's private one (DESIGN 8): another user who
/// created it first must never receive a request. A missing directory means no hub.
pub(crate) fn check_socket(path: &Path) -> Result<(), ClientError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("/"));
    match crate::sys::fs::check_private_dir(dir) {
        Ok(()) => Ok(()),
        Err(FsError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            Err(ClientError::NoHub)
        }
        Err(e) => Err(ClientError::Unsafe(e.to_string())),
    }
}

/// The launch token this process carries, when it runs inside a session the hub started.
pub(crate) fn launch_token() -> Option<String> {
    std::env::var("CATALY_LAUNCH")
        .ok()
        .filter(|t| !t.is_empty())
}

pub(crate) struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    launch: Option<String>,
}

impl Client {
    pub(crate) fn connect(path: &Path) -> Result<Self, ClientError> {
        check_socket(path)?;
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
            launch: launch_token(),
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
            launch: self.launch.as_deref(),
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn a_socket_outside_a_private_directory_is_never_used() {
        let scratch = crate::sys::fs::tests::scratch("client");
        assert!(check_socket(&scratch.join("hub.sock")).is_ok());
        assert!(matches!(
            check_socket(&scratch.join("absent/hub.sock")),
            Err(ClientError::NoHub)
        ));
        let open = scratch.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            check_socket(&open.join("hub.sock")),
            Err(ClientError::Unsafe(_))
        ));
        let link = scratch.join("link");
        std::os::unix::fs::symlink(&*scratch, &link).unwrap();
        assert!(matches!(
            check_socket(&link.join("hub.sock")),
            Err(ClientError::Unsafe(_))
        ));
        assert!(matches!(
            Client::connect(&open.join("hub.sock")),
            Err(ClientError::Unsafe(_))
        ));
    }
}
