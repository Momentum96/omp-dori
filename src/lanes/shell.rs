//! Shell rules (LANE-30, EXT-31; LANE req. 72-75; DESIGN 9.1-9.2): the one shell tokeniser, the
//! built-in rule 0 every session's shell obeys (the lead's, each lane's and their subagents'), and
//! for lanes the snapshot taken at open, the verdict on one command and one-attempt grants. Rule 0
//! cannot be granted: no `omp usage reset`, no `omp config set` of a reset setting, and no `cataly`
//! verb beyond the read-only ones. It guards against mistakes, not against deliberate escape; the
//! control socket's roles are the boundary (DESIGN 8).

use std::collections::HashMap;

use super::record::Grant;

/// `cataly` verbs a session's shell may run (DESIGN 8).
const READ_ONLY: [&str; 5] = ["status", "events", "stats", "version", "help"];
/// `cataly` verbs that change something; after a command word the hub cannot resolve, one of them
/// fails closed.
const WRITES: [&str; 19] = [
    "config",
    "discord",
    "hub",
    "install",
    "job",
    "lane",
    "lead",
    "pr",
    "quota",
    "rollback",
    "service",
    "setup",
    "slack",
    "slot",
    "telegram",
    "uninstall",
    "update",
    "voice",
    "home",
];
/// Commands and shell keywords that run another command named among their arguments: after one of
/// them every word of the segment is judged as a possible command.
const RUNNERS: [&str; 33] = [
    "env", "command", "exec", "nohup", "sudo", "doas", "time", "nice", "ionice", "timeout",
    "stdbuf", "setsid", "xargs", "find", "watch", "flock", "parallel", "eval", "sh", "bash", "zsh",
    "dash", "ksh", "su", "then", "do", "else", "elif", "if", "while", "until", "!", "{",
];
/// Nesting depth for quoted commands and command substitutions.
const NESTING: usize = 4;

/// One shell line as simple-command segments of words, the way the shell splits it: `;`, `&`, `|`,
/// `(`, `)`, backquotes, `$(` and line breaks end a segment; quotes are removed and backslash
/// escapes applied; `${…}` stays in its word. A quoted word keeps its spaces.
pub(crate) fn segments(command: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    let end_word = |words: &mut Vec<String>, word: &mut String, started: &mut bool| {
        if *started {
            words.push(std::mem::take(word));
            *started = false;
        }
    };
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\')
                if chars
                    .peek()
                    .is_some_and(|n| matches!(n, '"' | '\\' | '$' | '`')) =>
            {
                word.extend(chars.next());
            }
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => (quote, started) = (Some(c), true),
            (None, '\\') => {
                if let Some(next) = chars.next().filter(|n| *n != '\n') {
                    word.push(next);
                    started = true;
                }
            }
            (None, '$') if chars.peek() == Some(&'{') => {
                word.push('$');
                for inner in chars.by_ref() {
                    word.push(inner);
                    if inner == '}' {
                        break;
                    }
                }
                started = true;
            }
            (None, c)
                if matches!(c, ';' | '&' | '|' | '(' | ')' | '`' | '\n')
                    || (c == '$' && chars.peek() == Some(&'(')) =>
            {
                end_word(&mut words, &mut word, &mut started);
                if !words.is_empty() {
                    out.push(std::mem::take(&mut words));
                }
            }
            (None, c) if c.is_whitespace() => end_word(&mut words, &mut word, &mut started),
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    end_word(&mut words, &mut word, &mut started);
    if !words.is_empty() {
        out.push(words);
    }
    out
}

/// `NAME=value` as (name, value), when `word` is an assignment.
fn assignment(word: &str) -> Option<(&str, &str)> {
    let (name, value) = word.split_once('=')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some((name, value))
}

/// `word` with `$NAME` and `${NAME}` replaced from the assignments seen so far; `None` when it still
/// expands something the hub cannot know (another variable, a substitution).
fn resolve(word: &str, vars: &HashMap<String, String>) -> Option<String> {
    let mut out = String::new();
    let mut rest = word;
    while let Some(at) = rest.find('$') {
        out.push_str(rest.get(..at)?);
        let after = rest.get(at + 1..)?;
        let (name, tail) = if let Some(braced) = after.strip_prefix('{') {
            let close = braced.find('}')?;
            (braced.get(..close)?, braced.get(close + 1..)?)
        } else {
            let len = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            (after.get(..len)?, after.get(len..)?)
        };
        out.push_str(vars.get(name)?);
        rest = tail;
    }
    out.push_str(rest);
    Some(out)
}

fn base(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Whether the words after `omp` ask it to spend a saved reset: `usage` and `reset` in any order, or
/// `config set` of a `*Resets*` setting.
fn omp_spends(args: &[String]) -> bool {
    let has = |w: &str| args.iter().any(|a| a == w);
    (has("usage") && has("reset"))
        || (has("config") && has("set") && args.iter().any(|a| a.to_lowercase().contains("resets")))
}

/// Whether the words after `cataly` change anything.
fn cataly_writes(args: &[String]) -> bool {
    let next = args.first().map_or("", String::as_str);
    !READ_ONLY.contains(&next) && !next.starts_with("--help") && next != "-h"
}

/// Rule 0 for one segment whose words are already resolved where they can be.
fn segment_blocked(raw: &[String], words: &[Option<String>], depth: usize) -> bool {
    let text = |w: &Option<String>| w.clone().unwrap_or_default();
    let rest = |from: usize| -> Vec<String> { words.iter().skip(from).map(text).collect() };
    // A substitution inside any word runs too, and so does a quoted command a runner is given.
    let nested = raw
        .iter()
        .any(|w| (w.contains("$(") || w.contains('`')) && blocked_at(w, depth + 1))
        || words
            .iter()
            .flatten()
            .any(|w| w.chars().any(char::is_whitespace) && blocked_at(w, depth + 1));
    let Some(first) = words.first() else {
        return nested;
    };
    // A command word the hub cannot resolve fails closed on anything mutating after it.
    let unknown_blocked = |i: usize| {
        let args = rest(i + 1);
        args.first().is_some_and(|w| WRITES.contains(&w.as_str())) || omp_spends(&args)
    };
    let Some(command) = first else {
        return nested || unknown_blocked(0);
    };
    let any_position = RUNNERS.contains(&base(command)) || command.starts_with(['<', '>']);
    let blocked_here = |i: usize, word: &str| match base(word) {
        "cataly" => cataly_writes(&rest(i + 1)),
        "omp" => omp_spends(&rest(i + 1)),
        _ => false,
    };
    nested
        || blocked_here(0, command)
        || (any_position
            && words.iter().enumerate().skip(1).any(|(i, w)| match w {
                Some(w) => blocked_here(i, w),
                None => unknown_blocked(i),
            }))
}

fn blocked_at(command: &str, depth: usize) -> bool {
    if depth > NESTING {
        return true;
    }
    let mut vars = HashMap::new();
    segments(command).iter().any(|words| {
        let mut at = 0;
        while let Some((name, value)) = words.get(at).and_then(|w| assignment(w)) {
            if let Some(v) = resolve(value, &vars) {
                vars.insert(name.to_string(), v);
            } else {
                vars.remove(name);
            }
            at += 1;
        }
        let resolved: Vec<Option<String>> =
            words.iter().skip(at).map(|w| resolve(w, &vars)).collect();
        segment_blocked(words, &resolved, depth)
    })
}

/// The built-in rule 0 (DESIGN 9.2): whether `command` would spend a saved reset or run a `cataly`
/// verb that changes anything.
pub(crate) fn rule_zero(command: &str) -> bool {
    blocked_at(command, 0)
}

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
    if rule_zero(command) {
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

    #[test]
    fn one_tokeniser_splits_like_the_shell() {
        assert_eq!(
            segments(r#"a 'b c' "d\"e" x\ y; f|g && $(h i) `j`"#),
            [
                vec!["a", "b c", "d\"e", "x y"],
                vec!["f"],
                vec!["g"],
                vec!["h", "i"],
                vec!["j"]
            ]
        );
        assert_eq!(segments("echo ${a}b $c"), [vec!["echo", "${a}b", "$c"]]);
    }

    #[test]
    fn rule_zero_ignores_order_and_follows_indirection() {
        for cmd in [
            "omp usage --json reset",
            "omp usage -p anthropic reset",
            "omp config set claudeResets.autoRedeem5h yes",
            "c=cataly; $c lane close x",
            "c=cata; ${c}ly lane close x",
            "$TOOL lane close x",
            "env FOO=1 cataly quota lift",
            "sh -c 'cataly voice restart'",
            "echo \"$(cataly lead send hi)\"",
            "if true; then cataly discord release r; fi",
            "cata\\ly job enable a --ack",
            "x=1 /opt/bin/cataly setup --apply",
            "o=omp; $o usage reset",
        ] {
            assert!(rule_zero(cmd), "{cmd}");
        }
        for cmd in [
            "cataly status",
            "c=cataly; $c events",
            "omp usage --json",
            "omp config set theme dark",
            "git commit -m \"make cataly faster\"",
            "ls $HOME && $PYTHON -m pip install x",
        ] {
            assert!(!rule_zero(cmd), "{cmd}");
        }
    }
}
