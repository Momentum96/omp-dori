//! Per-lane shell rules (LANE-30, EXT-31; LANE req. 72-75; DESIGN 9.1-9.2): the snapshot taken at
//! open, the verdict on one command, and one-attempt grants. Rule 0 is built in and cannot be
//! granted: no `omp usage reset`, and no `cataly` verb beyond the read-only ones.

use super::record::Grant;

/// `cataly` verbs a lane's shell may run (DESIGN 8).
const READ_ONLY: [&str; 5] = ["status", "events", "stats", "version", "help"];

/// The verdict on one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) allowed: bool,
    /// Rule numbers that matched: 0 built in, 1.. the configured deny patterns.
    pub(crate) rules: Vec<usize>,
    /// SHA-256 of the command; the text itself is never stored.
    pub(crate) digest: String,
    /// Grants this attempt used up.
    pub(crate) used: Vec<usize>,
}

/// A lane-key glob: only `*` is special (LANE req. 72).
pub(crate) fn glob(pattern: &str, key: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (Some(first), Some(last)) = (parts.first(), parts.last()) else {
        return false;
    };
    if parts.len() == 1 {
        return pattern == key;
    }
    if !key.starts_with(first) || key.len() < first.len() + last.len() || !key.ends_with(last) {
        return false;
    }
    let mut at = first.len();
    let end = key.len() - last.len();
    for mid in parts.iter().skip(1).take(parts.len().saturating_sub(2)) {
        match key.get(at..end).and_then(|rest| rest.find(mid)) {
            Some(i) => at += i + mid.len(),
            None => return false,
        }
    }
    true
}

fn words(command: &str) -> Vec<&str> {
    command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '`' | '$'))
        .filter(|w| !w.is_empty())
        .map(|w| w.trim_matches(|c| c == '"' || c == '\''))
        .collect()
}

/// The built-in rule: never a saved-reset command, never a `cataly` verb that changes anything.
fn built_in(command: &str) -> bool {
    let w = words(command);
    let base = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    w.iter().enumerate().any(|(i, word)| {
        let next = w.get(i + 1).copied().unwrap_or("");
        match base(word).as_str() {
            "omp" => next == "usage" && w.get(i + 2) == Some(&"reset"),
            "cataly" => !READ_ONLY.contains(&next) && !next.starts_with("--help"),
            _ => false,
        }
    })
}

/// Judges one command against the lane's snapshot and grants; marks the grants an allowed attempt
/// uses (LANE req. 74-75).
pub(crate) fn judge(
    command: &str,
    deny: &[regex::Regex],
    allow: &[regex::Regex],
    grants: &mut [Grant],
) -> Verdict {
    let digest = crate::config::sha256_hex(command.as_bytes());
    let mut rules = Vec::new();
    if built_in(command) {
        rules.push(0);
    }
    let allowed_whole = allow.iter().any(|re| {
        re.find(command)
            .is_some_and(|m| m.start() == 0 && m.end() == command.len())
    });
    if !allowed_whole {
        rules.extend(
            deny.iter()
                .enumerate()
                .filter(|(_, re)| re.is_match(command))
                .map(|(i, _)| i + 1),
        );
    }
    let granted = |rule: usize, grants: &[Grant]| {
        grants
            .iter()
            .position(|g| !g.used && g.rule == rule && g.digest == digest)
    };
    let covered = !rules.is_empty()
        && !rules.contains(&0)
        && rules.iter().all(|r| granted(*r, grants).is_some());
    let mut used = Vec::new();
    if covered {
        for rule in &rules {
            if let Some(i) = granted(*rule, grants)
                && let Some(g) = grants.get_mut(i)
            {
                g.used = true;
                used.push(*rule);
            }
        }
    }
    Verdict {
        allowed: rules.is_empty() || covered,
        rules,
        digest,
        used,
    }
}

/// The blocker text the lane sees: rule numbers and a digest only (LANE req. 74, EXT req. 33).
pub(crate) fn blocker(v: &Verdict) -> String {
    let rules: Vec<String> = v
        .rules
        .iter()
        .map(|r| {
            if *r == 0 {
                "0 (built in)".into()
            } else {
                r.to_string()
            }
        })
        .collect();
    format!(
        "Blocked by shell rule {} (command sha256 {}). No command ran. Report a blocker \
         (kind waiting); only the lead or owner can grant one attempt.",
        rules.join(", "),
        v.digest.get(..16).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re(s: &str) -> regex::Regex {
        regex::Regex::new(s).unwrap()
    }

    #[test]
    fn deny_allow_built_ins_and_one_attempt_grants() {
        let deny = [re("git push"), re("rm -rf")];
        let allow = [re("git push origin feature")];
        let mut grants = Vec::new();
        assert!(judge("ls -la", &deny, &[], &mut grants).allowed);
        let v = judge("git push --force && rm -rf /", &deny, &[], &mut grants);
        assert!(!v.allowed);
        assert_eq!(v.rules, [1, 2]);
        assert!(!blocker(&v).contains("git push"));
        assert!(judge("git push origin feature", &deny, &allow, &mut grants).allowed);
        assert!(
            !judge(
                "git push origin feature; rm -rf /",
                &deny,
                &allow,
                &mut grants
            )
            .allowed
        );
        for cmd in [
            "omp usage reset",
            "/usr/bin/cataly lane close x",
            "x; cataly lead stop",
        ] {
            assert_eq!(judge(cmd, &deny, &[], &mut grants).rules, [0], "{cmd}");
        }
        assert!(judge("cataly status", &deny, &[], &mut grants).allowed);
        // One grant per matching rule, used up by the next identical attempt.
        let cmd = "git push origin main";
        let digest = judge(cmd, &deny, &[], &mut grants).digest;
        grants.push(Grant {
            rule: 1,
            digest,
            reason: "ok".into(),
            used: false,
        });
        let first = judge(cmd, &deny, &[], &mut grants);
        assert!(first.allowed && first.used == [1]);
        assert!(!judge(cmd, &deny, &[], &mut grants).allowed);
        assert!(glob("web-*", "web-1") && glob("*", "x") && !glob("web-*", "api-1"));
        assert!(glob("a*b*c", "a-b-c") && !glob("a*b*c", "a-c"));
    }
}
