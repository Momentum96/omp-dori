//! Enrolment (design 2.2, 2.3): the hub's per-worker key, the worker's forced-command line in
//! `authorized_keys`, the host-key pin in Cataly's own `known_hosts`, and `tailscale ip`. Parsers and
//! renderers are pure; the few commands they need (`ssh-keygen`, `ssh-keyscan`, `tailscale`) run
//! through the one bounded runner.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::sys::command::{self, Output, Spec};

/// Where a worker is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Destination {
    pub(crate) user: String,
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl Destination {
    /// `user@host`, `user@host:port`, or `user@[v6]:port`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let bad = || format!("'{text}' is not user@host or user@host:port");
        let (user, rest) = text.split_once('@').ok_or_else(bad)?;
        let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
            let (host, after) = inner.split_once(']').ok_or_else(bad)?;
            match after {
                "" => (host, None),
                p => (host, Some(p.strip_prefix(':').ok_or_else(bad)?)),
            }
        } else {
            match rest.rsplit_once(':') {
                Some((h, p)) if !h.contains(':') => (h, Some(p)),
                Some(_) => return Err(bad()),
                None => (rest, None),
            }
        };
        let port = match port {
            None => 22,
            Some(p) => p
                .parse::<u16>()
                .ok()
                .filter(|p| *p > 0)
                .ok_or_else(|| format!("'{p}' is not a port"))?,
        };
        if !word(user) {
            return Err(format!("'{user}' is not a user name"));
        }
        if !word(host) {
            return Err(format!("'{host}' is not a host name or address"));
        }
        Ok(Self {
            user: user.to_string(),
            host: host.to_string(),
            port,
        })
    }

    /// `user@host` as `ssh` takes it.
    pub(crate) fn target(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// The `known_hosts` name: `host`, or `[host]:port` on another port (sshd(8)).
    pub(crate) fn pin_name(&self) -> String {
        if self.port == 22 {
            self.host.clone()
        } else {
            format!("[{}]:{}", self.host, self.port)
        }
    }
}

/// A user or host word: never an option, never anything a shell or `ssh` reads specially.
fn word(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 253
        && !text.starts_with('-')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%'))
}

/// A worker name: as job names, at most 64 characters.
pub(crate) fn name_ok(name: &str) -> bool {
    crate::config::valid_name(name) && name.len() <= 64 && !name.contains('@')
}

/// The hub's ID in worker lines and roots: six hex digits, made once. The config folder is
/// Cataly's own, so a loose mode on it is tightened as `cataly setup` does.
pub(crate) fn hub_id(config_dir: &Path) -> Result<String, String> {
    crate::sys::fs::claim_private_dir(config_dir).map_err(|e| e.to_string())?;
    let path = config_dir.join("hub-id");
    match crate::sys::fs::read_private(&path, 64) {
        Ok(Some(bytes)) => {
            let id = String::from_utf8_lossy(&bytes).trim().to_string();
            if hub_id_ok(&id) {
                return Ok(id);
            }
            Err(format!("{} does not hold a hub ID", path.display()))
        }
        Ok(None) => {
            let id = crate::host::random_hex(3);
            crate::sys::fs::create_atomic(&path, format!("{id}\n").as_bytes())
                .map_err(|e| e.to_string())?;
            Ok(id)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Six lowercase hex digits.
pub(crate) fn hub_id_ok(id: &str) -> bool {
    id.len() == 6 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `<config>/secrets/worker-<name>`, the hub's private key for one worker.
pub(crate) fn key_path(config_dir: &Path, name: &str) -> PathBuf {
    config_dir.join("secrets").join(format!("worker-{name}"))
}

/// `<config>/known_hosts`, the pins of every worker.
pub(crate) fn known_hosts(config_dir: &Path) -> PathBuf {
    config_dir.join("known_hosts")
}

/// The comment that names this hub's key on a worker.
pub(crate) fn comment(hub: &str) -> String {
    format!("cataly-hub-{hub}")
}

async fn run(argv: &[String], timeout: Duration) -> Result<String, String> {
    let program = argv.first().cloned().unwrap_or_default();
    let done = command::run(&Spec {
        argv,
        cwd: None,
        env: &[],
        timeout,
        kill_grace: Duration::from_secs(2),
        output_cap: 64 * 1024,
        output: Output::Capture,
    })
    .await
    .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&done.stdout).into_owned();
    if done.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&done.stderr);
    let why = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map_or_else(
            || {
                if done.timed_out {
                    "timed out".to_string()
                } else {
                    format!("ended with {:?}", done.exit)
                }
            },
            |l| l.trim().to_string(),
        );
    Err(format!("{program}: {why}"))
}

/// Makes the worker's key (ed25519, no passphrase, owner-only) unless it exists; returns the public
/// key line.
pub(crate) async fn ensure_key(path: &Path, hub: &str) -> Result<String, String> {
    if std::fs::symlink_metadata(path).is_err() {
        if let Some(dir) = path.parent() {
            crate::sys::fs::claim_private_dir(dir).map_err(|e| e.to_string())?;
        }
        let argv: Vec<String> = [
            "ssh-keygen",
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            &comment(hub),
            "-f",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .chain([path.display().to_string()])
        .collect();
        run(&argv, Duration::from_secs(10)).await?;
    }
    let mut public = path.as_os_str().to_owned();
    public.push(".pub");
    let text = std::fs::read_to_string(&public)
        .map_err(|e| format!("{}: {e}", public.to_string_lossy()))?;
    let key = HubKey::parse(text.trim())?;
    Ok(key.line())
}

/// The hub's public key as `worker init` takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HubKey {
    pub(crate) blob: String,
    pub(crate) hub: String,
}

impl HubKey {
    /// `ssh-ed25519 <base64> cataly-hub-<id>`.
    pub(crate) fn parse(line: &str) -> Result<Self, String> {
        let mut parts = line.split_whitespace();
        let (Some(kind), Some(blob), Some(comment), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("expected 'ssh-ed25519 AAAA… cataly-hub-<id>'".into());
        };
        if kind != "ssh-ed25519" {
            return Err(format!("a {kind} key; Cataly uses ssh-ed25519 keys"));
        }
        let bytes = crate::sys::base64::decode(blob).ok_or("the key is not valid base64")?;
        if key_type(&bytes) != Some("ssh-ed25519") {
            return Err("the key's body is not an ed25519 key".into());
        }
        let hub = comment
            .strip_prefix("cataly-hub-")
            .filter(|id| hub_id_ok(id))
            .ok_or("the key's comment is not cataly-hub-<id>; copy the line worker add printed")?;
        Ok(Self {
            blob: blob.to_string(),
            hub: hub.to_string(),
        })
    }

    pub(crate) fn line(&self) -> String {
        format!("ssh-ed25519 {} {}", self.blob, comment(&self.hub))
    }
}

/// The key type an SSH wire-format public key starts with.
fn key_type(blob: &[u8]) -> Option<&str> {
    let len = usize::try_from(u32::from_be_bytes(blob.get(..4)?.try_into().ok()?)).ok()?;
    std::str::from_utf8(blob.get(4..4 + len)?).ok()
}

/// The worker's `authorized_keys` line (design 2.2): `restrict` takes away forwarding, PTY and rc;
/// `command=` runs worker mode whatever the client asks; `from=` limits the key to the hub's
/// addresses when given.
pub(crate) fn authorized_line(
    cataly: &Path,
    key: &HubKey,
    from: &[String],
) -> Result<String, String> {
    let program = cataly.display().to_string();
    if program.contains(['"', '\\', '\n']) || program.contains(char::is_whitespace) {
        return Err(format!(
            "{program} has a space or quote; install cataly under a plain path"
        ));
    }
    if from.iter().any(|f| {
        f.is_empty()
            || !f
                .chars()
                .all(|c| c.is_ascii_hexdigit() || matches!(c, '.' | ':' | '/' | '*' | '?'))
    }) {
        return Err("--from takes addresses separated by commas".into());
    }
    let from = if from.is_empty() {
        String::new()
    } else {
        format!(",from=\"{}\"", from.join(","))
    };
    Ok(format!(
        "restrict{from},command=\"{program} worker serve --hub {}\" {}",
        key.hub,
        key.line()
    ))
}

/// `authorized_keys` text with this hub's line replaced by `line` (or removed with `None`); every
/// other line is kept as it is. Applying the same line twice changes nothing.
pub(crate) fn replace_line(text: &str, hub: &str, line: Option<&str>) -> String {
    let tag = comment(hub);
    let mut out: Vec<&str> = text
        .lines()
        .filter(|l| l.split_whitespace().last() != Some(tag.as_str()))
        .collect();
    if let Some(line) = line {
        out.push(line);
    }
    let mut joined = out.join("\n");
    if !joined.is_empty() {
        joined.push('\n');
    }
    joined
}

/// OpenSSH's fingerprint of a base64 key body: `SHA256:` and unpadded base64.
pub(crate) fn fingerprint(blob: &str) -> Option<String> {
    let bytes = crate::sys::base64::decode(blob)?;
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let text = crate::sys::base64::encode(digest.as_ref());
    Some(format!("SHA256:{}", text.trim_end_matches('=')))
}

/// The ed25519 host key in `ssh-keyscan` output or a `.pub` file: its base64 body.
pub(crate) fn ed25519_key(text: &str) -> Option<String> {
    text.lines().filter(|l| !l.starts_with('#')).find_map(|l| {
        let mut words = l.split_whitespace();
        let mut prev = words.next()?;
        for w in words {
            if prev == "ssh-ed25519" {
                return Some(w.to_string());
            }
            prev = w;
        }
        None
    })
}

/// `known_hosts` text with the pin for `name` set to the ed25519 key `blob`; other pins are kept.
pub(crate) fn set_pin(text: &str, name: &str, blob: &str) -> String {
    let mut out: Vec<String> = text
        .lines()
        .filter(|l| l.split_whitespace().next() != Some(name))
        .map(str::to_string)
        .collect();
    out.push(format!("{name} ssh-ed25519 {blob}"));
    let mut joined = out.join("\n");
    joined.push('\n');
    joined
}

/// `known_hosts` text without the pin for `name`.
pub(crate) fn drop_pin(text: &str, name: &str) -> String {
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| l.split_whitespace().next() != Some(name))
        .collect();
    let mut joined = kept.join("\n");
    if !joined.is_empty() {
        joined.push('\n');
    }
    joined
}

/// Fetches the worker's ed25519 host key with `ssh-keyscan`.
pub(crate) async fn scan(dest: &Destination) -> Result<String, String> {
    let argv: Vec<String> = [
        "ssh-keyscan",
        "-T",
        "10",
        "-t",
        "ed25519",
        "-p",
        &dest.port.to_string(),
        "--",
        &dest.host,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    let out = run(&argv, Duration::from_secs(20)).await?;
    ed25519_key(&out).ok_or_else(|| format!("{} offered no ed25519 host key", dest.pin_name()))
}

/// Whether `ip` is a tailnet IPv4 address (`100.64.0.0/10`).
pub(crate) fn in_tailnet(ip: &str) -> bool {
    ip.parse::<Ipv4Addr>()
        .is_ok_and(|a| a.octets()[0] == 100 && (a.octets()[1] & 0xC0) == 64)
}

/// The first address `tailscale ip` printed.
pub(crate) fn first_address(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// `tailscale ip -4 <machine>`: the tailnet IPv4 address of a machine, refused off the tailnet.
pub(crate) async fn tailscale_ip(machine: &str) -> Result<String, String> {
    if in_tailnet(machine) {
        return Ok(machine.to_string());
    }
    if machine.parse::<std::net::IpAddr>().is_ok() {
        return Err(format!("{machine} is not on the tailnet (100.64.0.0/10)"));
    }
    let argv = ["tailscale", "ip", "-4", machine].map(String::from);
    let out = run(&argv, Duration::from_secs(10))
        .await
        .map_err(|e| format!("{machine} is not on the tailnet ({e})"))?;
    match first_address(&out) {
        Some(ip) if in_tailnet(&ip) => Ok(ip),
        _ => Err(format!("{machine} is not on the tailnet")),
    }
}

/// The hub's own tailnet addresses (`tailscale ip`), for the worker line's `from=`.
pub(crate) async fn own_tailscale_ips() -> Result<Vec<String>, String> {
    let argv = ["tailscale", "ip"].map(String::from);
    let out = run(&argv, Duration::from_secs(10)).await?;
    let ips: Vec<String> = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if ips.is_empty() {
        return Err("tailscale ip printed no address; is this machine on the tailnet?".into());
    }
    Ok(ips)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOB: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIEm0bLR8b7q3bH6oOCoVHuL2t4WQ4lcRrlrmNGa0wAPE";

    #[test]
    fn destinations_parse_and_pin() {
        let d = Destination::parse("dev@linux-box").unwrap();
        assert_eq!(
            (d.target(), d.port, d.pin_name()),
            ("dev@linux-box".into(), 22, "linux-box".into())
        );
        let d = Destination::parse("dev@100.101.102.103:2222").unwrap();
        assert_eq!(d.pin_name(), "[100.101.102.103]:2222");
        let d = Destination::parse("dev@[fd7a::1]:2200").unwrap();
        assert_eq!((d.host.as_str(), d.port), ("fd7a::1", 2200));
        for bad in [
            "linux-box",
            "dev@",
            "@h",
            "dev@-oProxyCommand=x",
            "dev@h:0",
            "dev@h:x",
            "d v@h",
            "dev@h;rm",
        ] {
            assert!(Destination::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_key_line_round_trips_and_names_the_hub() {
        let line = format!("ssh-ed25519 {BLOB} cataly-hub-3f2a9c");
        let key = HubKey::parse(&line).unwrap();
        assert_eq!((key.hub.as_str(), key.line()), ("3f2a9c", line));
        assert!(HubKey::parse(&format!("ssh-rsa {BLOB} cataly-hub-3f2a9c")).is_err());
        assert!(HubKey::parse(&format!("ssh-ed25519 {BLOB} someone@else")).is_err());
        assert!(HubKey::parse("ssh-ed25519 !!!! cataly-hub-3f2a9c").is_err());
    }

    #[test]
    fn the_authorized_line_is_restricted_and_replaced_in_place() {
        let key = HubKey::parse(&format!("ssh-ed25519 {BLOB} cataly-hub-3f2a9c")).unwrap();
        let line = authorized_line(Path::new("/home/dev/.local/bin/cataly"), &key, &[]).unwrap();
        assert_eq!(
            line,
            format!(
                "restrict,command=\"/home/dev/.local/bin/cataly worker serve --hub 3f2a9c\" ssh-ed25519 {BLOB} cataly-hub-3f2a9c"
            )
        );
        let from = vec!["100.64.1.2".to_string(), "fd7a::2".to_string()];
        let tailnet = authorized_line(Path::new("/c"), &key, &from).unwrap();
        assert!(tailnet.starts_with("restrict,from=\"100.64.1.2,fd7a::2\",command="));
        assert!(authorized_line(Path::new("/a b/cataly"), &key, &[]).is_err());
        assert!(authorized_line(Path::new("/c"), &key, &["x\" y".to_string()]).is_err());
        let existing = "ssh-ed25519 OTHER me@laptop\n";
        let once = replace_line(existing, "3f2a9c", Some(&line));
        assert_eq!(replace_line(&once, "3f2a9c", Some(&line)), once);
        assert!(once.starts_with(existing));
        let moved = replace_line(&once, "3f2a9c", Some(&tailnet));
        assert_eq!(moved.lines().count(), 2);
        assert_eq!(replace_line(&moved, "3f2a9c", None), existing);
        assert_eq!(replace_line("", "3f2a9c", None), "");
    }

    #[test]
    fn fingerprints_and_pins() {
        // `ssh-keygen -lf` of this key prints this fingerprint.
        let fp = fingerprint(BLOB).unwrap();
        assert!(fp.starts_with("SHA256:") && !fp.ends_with('='));
        assert_eq!(fp.len(), "SHA256:".len() + 43);
        let scanned = format!("# linux-box:22 SSH-2.0-OpenSSH_9.6\nlinux-box ssh-ed25519 {BLOB}\n");
        assert_eq!(ed25519_key(&scanned).as_deref(), Some(BLOB));
        assert_eq!(
            ed25519_key(&format!("ssh-ed25519 {BLOB} root@box\n")).as_deref(),
            Some(BLOB)
        );
        assert_eq!(ed25519_key("box ssh-rsa AAAA\n"), None);
        let pins = set_pin(
            "other ssh-ed25519 X\n[h]:2222 ssh-ed25519 OLD\n",
            "[h]:2222",
            BLOB,
        );
        assert_eq!(
            pins,
            format!("other ssh-ed25519 X\n[h]:2222 ssh-ed25519 {BLOB}\n")
        );
        assert_eq!(drop_pin(&pins, "[h]:2222"), "other ssh-ed25519 X\n");
        assert_eq!(drop_pin(&pins, "h"), pins);
    }

    #[test]
    fn tailnet_addresses() {
        for ip in ["100.64.0.1", "100.101.102.103", "100.127.255.254"] {
            assert!(in_tailnet(ip), "{ip}");
        }
        for ip in [
            "100.63.255.255",
            "100.128.0.1",
            "10.0.0.1",
            "linux-box",
            "fd7a:115c::1",
        ] {
            assert!(!in_tailnet(ip), "{ip}");
        }
        assert_eq!(
            first_address("\n100.101.102.103\n").as_deref(),
            Some("100.101.102.103")
        );
        assert_eq!(first_address(""), None);
    }

    #[test]
    fn names_and_ids() {
        assert!(name_ok("linux-box") && name_ok("mac.mini_2"));
        assert!(!name_ok("") && !name_ok("-x") && !name_ok("a@b") && !name_ok(&"x".repeat(65)));
        assert!(hub_id_ok("3f2a9c") && !hub_id_ok("3F2A9C") && !hub_id_ok("3f2a9"));
    }
}
