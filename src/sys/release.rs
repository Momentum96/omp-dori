//! What a release is and how one is proven genuine (IR req. 1-6, 23; DESIGN 11.1, 11.4): the version
//! report, asset names, the checksum list, its ed25519 signature, the executable format, and the
//! GitHub release metadata the update check and `cataly update` read.

use std::fmt;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::relay::http::{self, Base, Kind, Outcome, Policy, Request};

/// The product version, from the package manifest (IR req. 6).
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The commit the release workflow built, when it said so.
pub(crate) const COMMIT: &str = match option_env!("CATALY_COMMIT") {
    Some(c) => c,
    None => "unknown",
};
/// This binary's release target.
pub(crate) const TARGET: &str = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    "linux-x64"
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    "darwin-arm64"
} else {
    "unsupported"
};
/// The canonical repository releases come from.
pub(crate) const REPOSITORY: &str = "vntrevx/Cataly";
/// GitHub's API, unless the config points the check at a loopback fake.
pub(crate) const GITHUB_API: &str = "https://api.github.com";
/// The checksum list and its signature, named the same in every release (DESIGN 11.1).
pub(crate) const SUMS: &str = "SHA256SUMS";
pub(crate) const SIGNATURE: &str = "SHA256SUMS.sig";
/// The ed25519 public key releases are signed with, compiled in by the release workflow
/// (`CATALY_RELEASE_KEY`, 64 hex digits). A build without one reads the variable at run time.
const RELEASE_KEY: Option<&str> = option_env!("CATALY_RELEASE_KEY");

/// Bounds (IR req. 9, 23, 32).
pub(crate) const SUMS_MAX: u64 = 64 * 1024;
pub(crate) const EXTENSION_MAX: u64 = 1024 * 1024;
pub(crate) const BINARY_MAX: u64 = 128 * 1024 * 1024;
pub(crate) const METADATA_MAX: usize = 1024 * 1024;
pub(crate) const SIGNATURE_MAX: u64 = 1024;
/// The candidate's version report: 5 s, 64 KiB.
pub(crate) const REPORT_TIMEOUT: Duration = Duration::from_secs(5);
const REPORT_CAP: u64 = 64 * 1024;
/// GitHub metadata: 15 s in all.
pub(crate) const METADATA_TIMEOUT: Duration = Duration::from_secs(15);
/// One asset download.
pub(crate) const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// `cataly version --json` (IR req. 5): needs no configuration and starts nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Report {
    pub(crate) version: String,
    pub(crate) commit: String,
    pub(crate) target: String,
    pub(crate) omp_minimum: String,
    /// The bridge protocol the extension file speaks.
    pub(crate) extension_protocol: u64,
    /// The binary has `cataly service refresh`, which update and rollback run before they start
    /// it through the user service. Reports from earlier versions lack it.
    #[serde(default)]
    pub(crate) service_refresh: bool,
}

/// This binary's report.
pub(crate) fn report() -> Report {
    Report {
        version: VERSION.into(),
        commit: COMMIT.into(),
        target: TARGET.into(),
        omp_minimum: crate::omp::MINIMUM.to_string(),
        extension_protocol: crate::hub::bridge::PROTOCOL,
        service_refresh: true,
    }
}

/// The binary asset of `version` for this target.
pub(crate) fn binary_asset(version: &str) -> String {
    format!("cataly-{version}-{TARGET}")
}

/// The extension asset of `version`, the same file for every target.
pub(crate) fn extension_asset(version: &str) -> String {
    format!("cataly-{version}.ts")
}

/// A product version, `MAJOR.MINOR.PATCH` with an optional pre-release suffix (IR req. 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version {
    pub(crate) core: (u64, u64, u64),
    pub(crate) pre: Option<String>,
}

impl Version {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let (core, pre) = match text.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (text, None),
        };
        let mut parts = core.split('.');
        let mut number = || {
            parts
                .next()
                .filter(|p| !p.is_empty() && p.len() <= 9 && p.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|p| p.parse::<u64>().ok())
        };
        let core = (number()?, number()?, number()?);
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            Some(p)
                if !p.is_empty()
                    && p.len() <= 32
                    && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') =>
            {
                Some(p.to_string())
            }
            Some(_) => return None,
            None => None,
        };
        Some(Self { core, pre })
    }

    pub(crate) fn stable(&self) -> bool {
        self.pre.is_none()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (a, b, c) = self.core;
        write!(f, "{a}.{b}.{c}")?;
        if let Some(p) = &self.pre {
            write!(f, "-{p}")?;
        }
        Ok(())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    /// Numeric by major, minor, patch; a pre-release sorts before its release.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

/// The checksum list: asset name to lower-case hex SHA-256 (IR req. 3). A name listed twice is kept
/// as ambiguous so that "exactly one line" can be checked.
#[derive(Debug, Default)]
pub(crate) struct Sums(Vec<(String, String)>);

impl Sums {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let mut lines = Vec::new();
        for (n, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let (hash, name) = line
                .split_once("  ")
                .or_else(|| line.split_once(" *"))
                .ok_or(format!("{SUMS} line {} is not '<sha256>  <name>'", n + 1))?;
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || name.is_empty() {
                return Err(format!("{SUMS} line {} is not '<sha256>  <name>'", n + 1));
            }
            lines.push((name.to_string(), hash.to_ascii_lowercase()));
        }
        Ok(Self(lines))
    }

    /// The one hash listed for `name`.
    pub(crate) fn one(&self, name: &str) -> Result<&str, String> {
        let mut found = self.0.iter().filter(|(n, _)| n == name);
        match (found.next(), found.next()) {
            (Some((_, hash)), None) => Ok(hash),
            (None, _) => Err(format!("{SUMS} has no line for {name}")),
            (Some(_), Some(_)) => Err(format!("{SUMS} lists {name} more than once")),
        }
    }

    /// The version whose binary for this target the list names; exactly one must be listed.
    pub(crate) fn version(&self) -> Result<String, String> {
        let suffix = format!("-{TARGET}");
        let mut versions = self.0.iter().filter_map(|(n, _)| {
            n.strip_prefix("cataly-")
                .and_then(|rest| rest.strip_suffix(suffix.as_str()))
                .filter(|v| Version::parse(v).is_some())
        });
        match (versions.next(), versions.next()) {
            (Some(v), None) => Ok(v.to_string()),
            (None, _) => Err(format!("{SUMS} lists no binary for {TARGET}")),
            (Some(_), Some(_)) => Err(format!("{SUMS} lists more than one binary for {TARGET}")),
        }
    }
}

/// The key releases must be signed with: compiled in, else `CATALY_RELEASE_KEY` for a local build.
pub(crate) fn release_key() -> Result<[u8; 32], String> {
    let text = match RELEASE_KEY {
        Some(k) => k.to_string(),
        None => std::env::var("CATALY_RELEASE_KEY").map_err(|_| {
            "this build has no release key compiled in; set CATALY_RELEASE_KEY to the public key \
             (64 hex digits) the assets were signed with"
                .to_string()
        })?,
    };
    let bytes = unhex(text.trim()).ok_or("the release key is not 64 hex digits")?;
    bytes
        .try_into()
        .map_err(|_| "the release key is not 64 hex digits".to_string())
}

/// Checks the detached ed25519 signature (hex text) of the checksum list.
pub(crate) fn verify_signature(sums: &[u8], signature: &str, key: &[u8; 32]) -> Result<(), String> {
    let sig = unhex(signature.trim()).ok_or(format!("{SIGNATURE} is not hex text"))?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(sums, &sig)
        .map_err(|_| format!("{SIGNATURE} does not match {SUMS} and the release key"))
}

pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digit = |b: u8| char::from(b).to_digit(16);
            match pair {
                [h, l] => Some(u8::try_from(digit(*h)? * 16 + digit(*l)?).ok()?),
                _ => None,
            }
        })
        .collect()
}

/// The executable format of this host (IR req. 23): ELF 64-bit x86-64 on Linux, Mach-O 64-bit arm64
/// on macOS. `head` is the file's first bytes.
pub(crate) fn check_format(head: &[u8]) -> Result<(), String> {
    let elf = head.get(..4) == Some(b"\x7fELF".as_slice())
        && head.get(4) == Some(&2)
        && head.get(5) == Some(&1)
        && head.get(18..20) == Some([0x3e, 0x00].as_slice());
    let macho = head.get(..4) == Some([0xcf, 0xfa, 0xed, 0xfe].as_slice())
        && head.get(4..8) == Some([0x0c, 0x00, 0x00, 0x01].as_slice());
    let ok = match TARGET {
        "linux-x64" => elf,
        "darwin-arm64" => macho,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!("the binary is not an executable for {TARGET}"))
    }
}

/// SHA-256 of a file, read in pieces, refusing more than `cap` bytes.
pub(crate) fn sha256_file(path: &Path, cap: u64) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut reader = file.take(cap + 1);
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > cap {
            return Err(format!("{} is larger than {cap} bytes", path.display()));
        }
        ctx.update(buf.get(..n).unwrap_or_default());
    }
    Ok(super::hex(ctx.finish().as_ref()))
}

/// SHA-256 of bytes in memory.
pub(crate) fn sha256(bytes: &[u8]) -> String {
    super::hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// Runs `<binary> version --json` under the report bounds and reads its report.
pub(crate) async fn candidate_report(binary: &Path) -> Result<Report, String> {
    use crate::sys::command::{KILL_GRACE, Output, Spec, run};
    let argv = [
        binary.display().to_string(),
        "version".into(),
        "--json".into(),
    ];
    let done = run(&Spec {
        argv: &argv,
        cwd: None,
        env: &[],
        timeout: REPORT_TIMEOUT,
        kill_grace: KILL_GRACE,
        output_cap: REPORT_CAP,
        output: Output::Capture,
    })
    .await
    .map_err(|e| format!("the candidate binary did not run: {e}"))?;
    if done.timed_out || done.truncated || !done.success() {
        return Err("the candidate binary gave no version report".into());
    }
    serde_json::from_slice(&done.stdout)
        .map_err(|_| "the candidate binary's version report is unreadable".into())
}

/// One release as GitHub describes it.
#[derive(Debug, Clone)]
pub(crate) struct Release {
    pub(crate) tag: String,
    pub(crate) version: Version,
    /// Asset name and download URL.
    pub(crate) assets: Vec<(String, String)>,
}

impl Release {
    pub(crate) fn asset(&self, name: &str) -> Result<&str, String> {
        self.assets
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, url)| url.as_str())
            .ok_or(format!("release {} has no asset {name}", self.tag))
    }

    fn has(&self, name: &str) -> bool {
        self.assets.iter().any(|(n, _)| n == name)
    }

    /// A Cataly release carries its checksum list and its extension, which every target shares;
    /// the repository's releases of other software do not.
    fn is_cataly(&self) -> bool {
        self.has(SUMS) && self.has(&extension_asset(&self.version.to_string()))
    }

    /// Refuses a release that lacks one of the four assets this host installs.
    pub(crate) fn complete(&self) -> Result<(), String> {
        let v = self.version.to_string();
        let missing: Vec<String> = [
            SUMS.to_string(),
            SIGNATURE.to_string(),
            binary_asset(&v),
            extension_asset(&v),
        ]
        .into_iter()
        .filter(|n| !self.has(n))
        .collect();
        if missing.is_empty() {
            return Ok(());
        }
        Err(format!(
            "release {} is not a complete Cataly release for {TARGET}: it has no {}",
            self.tag,
            missing.join(", ")
        ))
    }
}

/// How many releases, newest first, the latest-release lookup reads.
const LISTED: u32 = 30;

/// Reads the newest stable Cataly release, or the release of `tag` (IR req. 8, 32): one request,
/// 15 s in all, at most 1 MiB. `Ok(None)` when no stable release exists yet. Either must carry
/// the four assets this host installs.
pub(crate) async fn fetch_release(
    api: &Base,
    tag: Option<&str>,
) -> Result<Option<Release>, String> {
    let path = match tag {
        Some(t) if Version::parse(t).is_some() && t.starts_with('v') => {
            format!("/repos/{REPOSITORY}/releases/tags/{t}")
        }
        Some(t) => return Err(format!("'{t}' is not a release tag such as v1.2.3")),
        None => format!("/repos/{REPOSITORY}/releases?per_page={LISTED}"),
    };
    let req = Request {
        method: hyper::Method::GET,
        path: &path,
        headers: vec![(
            hyper::header::ACCEPT,
            hyper::header::HeaderValue::from_static("application/vnd.github+json"),
        )],
        json: None,
        kind: Kind::Read,
    };
    let policy = Policy {
        attempt: METADATA_TIMEOUT,
        attempts: 1,
        budget: Duration::ZERO,
    };
    let outcome = tokio::time::timeout(METADATA_TIMEOUT, http::call(api, &req, policy, |_| {}))
        .await
        .map_err(|_| "GitHub gave no answer within 15 s".to_string())?;
    let body = match outcome {
        Outcome::Done(r) => r.body,
        Outcome::Rejected(r) if r.status == 404 => return Ok(None),
        Outcome::Rejected(r) => return Err(format!("GitHub answered HTTP {}", r.status)),
        Outcome::RateLimited(d) => {
            return Err(format!("GitHub asked to wait {} s", d.as_secs()));
        }
        Outcome::Uncertain(why) | Outcome::Failed(why) => return Err(format!("GitHub: {why}")),
    };
    if body.len() > METADATA_MAX {
        return Err("the release metadata is larger than 1 MiB".into());
    }
    if tag.is_some() {
        let release = parse_release(&body)?;
        release.complete()?;
        return Ok(Some(release));
    }
    latest(&body)
}

#[derive(Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct RawRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

impl RawRelease {
    fn release(self) -> Result<Release, String> {
        let version = Version::parse(&self.tag_name)
            .filter(|_| self.tag_name.starts_with('v'))
            .ok_or(format!("release tag '{}' is not v<version>", self.tag_name))?;
        if self.draft {
            return Err(format!("release {} is a draft", self.tag_name));
        }
        Ok(Release {
            tag: self.tag_name,
            version,
            assets: self
                .assets
                .into_iter()
                .map(|a| (a.name, a.browser_download_url))
                .collect(),
        })
    }
}

fn parse_release(body: &[u8]) -> Result<Release, String> {
    serde_json::from_slice::<RawRelease>(body)
        .map_err(|_| "the release metadata is unreadable".to_string())?
        .release()
}

/// The newest stable Cataly release in a release list. Drafts, prereleases, tags that are not
/// `v<version>` and releases of other software are skipped; the one chosen must be complete for
/// this host.
fn latest(body: &[u8]) -> Result<Option<Release>, String> {
    let list: Vec<RawRelease> =
        serde_json::from_slice(body).map_err(|_| "the release list is unreadable".to_string())?;
    let newest = list
        .into_iter()
        .filter(|r| !r.prerelease)
        .filter_map(|r| r.release().ok())
        .filter(|r| r.version.stable() && r.is_cataly())
        .max_by(|a, b| a.version.cmp(&b.version));
    match newest {
        Some(r) => r.complete().map(|()| Some(r)),
        None => Ok(None),
    }
}

/// Downloads one asset into `dest` (created new, owner-only), at most `cap` bytes, following
/// redirects to HTTPS only (IR req. 9).
pub(crate) async fn download(url: &str, cap: u64, dest: &Path) -> Result<(), String> {
    let name = dest
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let bytes = tokio::time::timeout(
        DOWNLOAD_TIMEOUT,
        http::download(url, usize::try_from(cap).unwrap_or(usize::MAX)),
    )
    .await
    .map_err(|_| format!("{name}: no complete download within 120 s"))?
    .map_err(|e| format!("{name}: {e}"))?;
    crate::sys::fs::create_atomic(dest, &bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        let v = |t: &str| Version::parse(t).unwrap();
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("1.0.0-rc.1"));
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert!(!v("1.0.0-rc.1").stable());
        for bad in ["1.2", "1.2.3.4", "1.x.3", "1.2.3-", "1.2.3-a b", ""] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
        assert_eq!(v("2.0.1-rc.2").to_string(), "2.0.1-rc.2");
    }

    #[test]
    fn sums_need_exactly_one_line() {
        let h = "ab".repeat(32);
        let text = format!(
            "{h}  {}\n{h}  cataly-1.2.0.ts\n{h}  dup\n{h}  dup\n",
            binary_asset("1.2.0")
        );
        let sums = Sums::parse(&text).unwrap();
        assert_eq!(sums.version().unwrap(), "1.2.0");
        assert_eq!(sums.one("cataly-1.2.0.ts").unwrap(), h);
        assert!(sums.one("missing").unwrap_err().contains("no line"));
        assert!(sums.one("dup").unwrap_err().contains("more than once"));
        assert!(Sums::parse("xyz  name").is_err());
        assert!(Sums::parse("").unwrap().version().is_err());
    }

    #[test]
    fn signatures_and_formats() {
        let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
        let key: [u8; 32] = ring::signature::KeyPair::public_key(&pair)
            .as_ref()
            .try_into()
            .unwrap();
        let sig = crate::sys::hex(pair.sign(b"list").as_ref());
        verify_signature(b"list", &sig, &key).unwrap();
        assert!(verify_signature(b"list2", &sig, &key).is_err());
        assert!(verify_signature(b"list", "zz", &key).is_err());
        let mut elf = vec![0x7f, b'E', b'L', b'F', 2, 1];
        elf.resize(18, 0);
        elf.extend([0x3e, 0]);
        let macho = [0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 1];
        let (good, bad): (&[u8], &[u8]) = if TARGET == "linux-x64" {
            (&elf, &macho)
        } else {
            (&macho, &elf)
        };
        check_format(good).unwrap();
        assert!(check_format(bad).is_err());
        assert!(check_format(b"#!/bin/sh\n").is_err());
        let own = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        check_format(own.get(..64).unwrap()).unwrap();
    }

    #[test]
    fn release_metadata() {
        let body = br#"{"tag_name":"v1.4.0","prerelease":false,"assets":[{"name":"SHA256SUMS","browser_download_url":"https://x/SHA256SUMS"}]}"#;
        let r = parse_release(body).unwrap();
        assert_eq!(r.version.to_string(), "1.4.0");
        assert_eq!(r.asset(SUMS).unwrap(), "https://x/SHA256SUMS");
        assert!(r.asset("nope").is_err());
        assert!(parse_release(br#"{"tag_name":"1.4.0"}"#).is_err());
        assert!(parse_release(br#"{"tag_name":"v1.4.0","draft":true}"#).is_err());
    }

    /// A release list entry with these asset names.
    fn entry(tag: &str, prerelease: bool, draft: bool, names: &[String]) -> serde_json::Value {
        let assets: Vec<_> = names
            .iter()
            .map(|n| serde_json::json!({"name": n, "browser_download_url": format!("https://x/{tag}/{n}")}))
            .collect();
        serde_json::json!({"tag_name": tag, "prerelease": prerelease, "draft": draft, "assets": assets})
    }

    fn full(version: &str) -> Vec<String> {
        vec![
            SUMS.into(),
            SIGNATURE.into(),
            binary_asset(version),
            extension_asset(version),
        ]
    }

    #[test]
    fn latest_skips_prereleases_drafts_and_other_software() {
        let foreign = vec!["other-linux-x64.tar.gz".to_string()];
        let list = serde_json::json!([
            entry("v0.9.0-rc.1", false, false, &full("0.9.0-rc.1")),
            entry("v0.8.0", true, false, &full("0.8.0")),
            entry("v0.7.0", false, true, &full("0.7.0")),
            entry("v5.0.0", false, false, &foreign),
            entry("v0.1.3", true, false, &foreign),
            entry("other-v9.0.0", false, false, &full("9.0.0")),
            entry("v0.2.0", false, false, &full("0.2.0")),
            entry("v0.1.9", false, false, &full("0.1.9")),
        ]);
        let r = latest(list.to_string().as_bytes()).unwrap().unwrap();
        assert_eq!(r.tag, "v0.2.0");
        assert_eq!(
            r.asset(SUMS).unwrap(),
            "https://x/v0.2.0/SHA256SUMS",
            "the chosen release's own assets"
        );

        let none = serde_json::json!([
            entry("v0.1.2", true, false, &foreign),
            entry("v0.1.3", false, false, &foreign)
        ]);
        assert!(latest(none.to_string().as_bytes()).unwrap().is_none());
        assert!(latest(b"[]").unwrap().is_none());
        assert!(latest(b"{}").is_err());
    }

    #[test]
    fn a_release_without_this_hosts_assets_is_refused() {
        let mut names = full("0.3.0");
        names.retain(|n| *n != binary_asset("0.3.0"));
        let list = serde_json::json!([entry("v0.3.0", false, false, &names)]);
        let why = latest(list.to_string().as_bytes()).unwrap_err();
        assert!(
            why.contains("v0.3.0") && why.contains(&binary_asset("0.3.0")),
            "{why}"
        );
        let one = entry("v0.3.0", false, false, &names[..2]).to_string();
        let why = parse_release(one.as_bytes())
            .unwrap()
            .complete()
            .unwrap_err();
        assert!(why.contains("cataly-0.3.0.ts"), "{why}");
    }
}
