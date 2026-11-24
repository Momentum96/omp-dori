//! Peer-credential check for the control socket (DESIGN 8, HOST req. 72).

use std::fmt;

use tokio::net::UnixStream;

/// The verified caller of a control connection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Peer {
    pub(crate) pid: Option<i32>,
}

/// Why a connection was refused; the hub closes it without a reply.
#[derive(Debug)]
pub(crate) enum PeerRefused {
    /// The OS could not report the peer's credentials.
    Unknown(std::io::Error),
    /// The peer runs as another user.
    OtherUser { uid: u32 },
}

impl fmt::Display for PeerRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(e) => write!(f, "peer credentials unavailable: {e}"),
            Self::OtherUser { uid } => write!(f, "peer runs as uid {uid}"),
        }
    }
}

/// Admits the peer only when its user ID equals `own_uid` (`SO_PEERCRED` on Linux, `getpeereid` on
/// macOS, through Tokio).
pub(crate) fn check_peer(stream: &UnixStream, own_uid: u32) -> Result<Peer, PeerRefused> {
    let cred = stream.peer_cred().map_err(PeerRefused::Unknown)?;
    if cred.uid() != own_uid {
        return Err(PeerRefused::OtherUser { uid: cred.uid() });
    }
    Ok(Peer { pid: cred.pid() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn same_user_admitted_other_user_refused() {
        let (a, _b) = UnixStream::pair().unwrap();
        let own = crate::sys::own_uid();
        let peer = check_peer(&a, own).unwrap();
        assert!(peer.pid.is_none_or(|p| p > 0));
        match check_peer(&a, own.wrapping_add(1)) {
            Err(PeerRefused::OtherUser { uid }) => assert_eq!(uid, own),
            other => panic!("expected refusal, got {other:?}"),
        }
    }
}
