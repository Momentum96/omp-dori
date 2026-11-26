//! `[workers.<name>]` (docs/design/workers.md 4.3): the machines that run lanes for this hub over SSH.
//! `cataly worker add` writes a section; `cataly worker remove` takes it out.

use std::collections::BTreeMap;

use super::{ConfigError, Section};
use crate::worker::enrol::{Destination, name_ok};

/// One worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkerSettings {
    /// `user@host`; with `tailscale`, the host is the tailnet IP.
    pub(crate) destination: String,
    pub(crate) port: u16,
    /// The name shown; the tailnet machine name with `tailscale`.
    pub(crate) name: Option<String>,
    /// `worker doctor` checks the address again with `tailscale ip`.
    pub(crate) tailscale: bool,
    /// Normalised Git origins (`github.com/owner/name`): placement evidence and doctor checks.
    pub(crate) repos: Vec<String>,
    /// Remote lanes at once.
    pub(crate) lanes: u32,
}

/// A normalised Git origin: `host/owner/name`, from an HTTPS, SSH or scp-like URL.
pub(crate) fn normalise_origin(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git") {
            return None;
        }
        let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
        // `host:port/path` keeps the host only.
        let (host, path) = rest.split_once('/')?;
        let host = host.split_once(':').map_or(host, |(h, _)| h);
        format!("{host}/{path}")
    } else if let Some((left, path)) = url.split_once(':') {
        // scp-like `user@host:owner/name`.
        let host = left.rsplit_once('@').map_or(left, |(_, h)| h);
        if host.is_empty() || host.contains('/') {
            return None;
        }
        format!("{host}/{path}")
    } else {
        url.to_string()
    };
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 3
        || parts.iter().any(|p| {
            *p == ".."
                || !p
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        })
    {
        return None;
    }
    let host = parts.first()?.to_ascii_lowercase();
    let path = parts.get(1..)?.join("/");
    Some(format!("{host}/{path}"))
}

pub(super) fn parse(
    errors: &mut Vec<ConfigError>,
    root: &mut Section<'_>,
    out: &mut BTreeMap<String, WorkerSettings>,
) {
    let Some(mut all) = root.table(errors, "workers") else {
        return;
    };
    for (name, value) in all.entries() {
        let path = all.child(name);
        let Some(table) = value.as_table() else {
            errors.push(ConfigError {
                path,
                message: "expected a table".into(),
            });
            continue;
        };
        if !name_ok(name) {
            errors.push(ConfigError {
                path: path.clone(),
                message:
                    "a worker name is a letter or digit, then letters, digits, '.', '_' or '-', \
                          64 at most"
                        .into(),
            });
        }
        let mut s = Section::new(path, table);
        let destination = s.required_string(errors, "destination").unwrap_or_default();
        let port = s
            .integer(errors, "port", 1, 65535)
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(22);
        if !destination.is_empty()
            && let Err(e) = Destination::parse(&format!("{destination}:{port}"))
        {
            errors.push(s.error("destination", &e));
        }
        let display = s.string(errors, "name").map(str::to_string);
        if display
            .as_deref()
            .is_some_and(|d| d.is_empty() || d.len() > 64)
        {
            errors.push(s.error("name", "expected 1 to 64 characters"));
        }
        let tailscale = s.bool(errors, "tailscale").unwrap_or(false);
        let mut repos = Vec::new();
        for (i, repo) in s
            .strings(errors, "repos")
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            match normalise_origin(repo) {
                Some(r) => repos.push(r),
                None => errors.push(ConfigError {
                    path: format!("{}[{i}]", s.child("repos")),
                    message: "expected a Git origin such as github.com/owner/name".into(),
                }),
            }
        }
        let lanes = s
            .integer(errors, "lanes", 1, i64::from(crate::worker::LANES_MAX))
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(1);
        s.finish(errors);
        out.insert(
            name.clone(),
            WorkerSettings {
                destination: destination.to_string(),
                port,
                name: display,
                tailscale,
                repos,
                lanes,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_normalise_from_every_url_form() {
        for url in [
            "https://github.com/vntrevx/cataly-core.git",
            "https://github.com/vntrevx/cataly-core",
            "https://token@GitHub.com/vntrevx/cataly-core/",
            "git@github.com:vntrevx/cataly-core.git",
            "ssh://git@github.com:22/vntrevx/cataly-core.git",
            "github.com/vntrevx/cataly-core",
        ] {
            assert_eq!(
                normalise_origin(url).as_deref(),
                Some("github.com/vntrevx/cataly-core"),
                "{url}"
            );
        }
        assert_eq!(
            normalise_origin("https://gitlab.com/group/sub/proj.git").as_deref(),
            Some("gitlab.com/group/sub/proj")
        );
        for bad in [
            "",
            "cataly-core",
            "ftp://h/o/n",
            "https://h/o",
            "file:///tmp/x",
            "h/../n/x",
        ] {
            assert_eq!(normalise_origin(bad), None, "{bad}");
        }
        // Normalising twice changes nothing.
        let once = normalise_origin("git@github.com:a/b.git").unwrap();
        assert_eq!(normalise_origin(&once).unwrap(), once);
    }

    #[test]
    fn a_worker_section_parses_and_is_checked() {
        let c = crate::config::parse(
            "[workers.linux-box]\ndestination = \"dev@100.101.102.103\"\nport = 2222\nname = \"linux-box\"\n\
             tailscale = true\nrepos = [\"https://github.com/vntrevx/cataly-core.git\"]\nlanes = 2\n",
        )
        .unwrap();
        let w = c.workers.get("linux-box").unwrap();
        assert_eq!((w.port, w.lanes, w.tailscale), (2222, 2, true));
        assert_eq!(w.repos, ["github.com/vntrevx/cataly-core"]);
        let c = crate::config::parse("[workers.b]\ndestination = \"dev@b\"\n").unwrap();
        assert_eq!((c.workers["b"].port, c.workers["b"].lanes), (22, 1));
        let errors = crate::config::parse(
            "[workers.b]\ndestination = \"-oProxyCommand=x\"\nlanes = 9\nrepos = [\"nope\"]\nextra = 1\n",
        )
        .unwrap_err();
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        for p in [
            "workers.b.destination",
            "workers.b.lanes",
            "workers.b.repos[0]",
            "workers.b.extra",
        ] {
            assert!(paths.contains(&p), "{p}: {paths:?}");
        }
        assert!(crate::config::parse("[workers.b]\nport = 22\n").is_err());
    }
}
