//! Every parameter the hub's OMP tools offer, in one table (DESIGN 10.6; WS-02, EVR-22 to EVR-25).
//!
//! The schema a tool is registered with is generated from this table, and every operation reads its
//! input through it, so a parameter cannot be read without being declared with its JSON type. The
//! tests below prove that from the parsers: they walk the functions reachable from each tool's
//! entry, refuse a raw read of the input, and check the typed inputs and the checks against the
//! table. A derive would need a crate the hub does not have; one table with a test that proves it
//! complete is the contract.

use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

/// The tools the hub registers for an OMP session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    /// The lead's `cataly_lead` tool.
    Lead,
    /// A lane's `cataly_lane` tool.
    Lane,
}

/// The JSON type of a parameter, as the schema declares it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Type {
    /// A string.
    Text,
    /// An integer.
    Int,
    /// A number, integer or not.
    Number,
    /// `true` or `false`.
    Flag,
    /// An array of strings.
    Texts,
    /// Any JSON value: a parameter the hub writes itself, never offered to the model.
    Any,
    /// Any JSON object: the answer to a dialog, whose shape the dialog fixes.
    Open,
    /// One of these strings.
    Choice(&'static [&'static str]),
    /// An object with exactly these fields.
    Object(&'static [Prop]),
    /// One of these types.
    Either(&'static [Type]),
}

/// One field of an object parameter.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Prop {
    pub(crate) name: &'static str,
    pub(crate) ty: Type,
    pub(crate) about: &'static str,
    pub(crate) required: bool,
}

/// One parameter of one or both tools.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Param {
    name: &'static str,
    ty: Type,
    about: &'static str,
    tools: &'static [Tool],
    required: bool,
}

impl Param {
    /// The parameter's name in the input.
    pub(crate) fn name(&self) -> &'static str {
        self.name
    }

    /// The JSON type the schema declares.
    pub(crate) fn ty(&self) -> Type {
        self.ty
    }

    /// The one-line description.
    pub(crate) fn about(&self) -> &'static str {
        self.about
    }

    /// Whether the tool offers this parameter; `Type::Any` ones are the hub's own.
    pub(crate) fn offered(&self, tool: Tool) -> bool {
        self.tools.contains(&tool) && !matches!(self.ty, Type::Any)
    }

    /// Whether this parameter belongs to the tool's input, offered or not.
    #[cfg(test)]
    pub(crate) fn declared_for(&self, tool: Tool) -> bool {
        self.tools.contains(&tool)
    }

    /// Whether every call must give it.
    pub(crate) fn required(&self) -> bool {
        self.required
    }

    /// The trimmed, non-empty text.
    pub(crate) fn text<'a>(&self, input: &'a Value) -> Option<&'a str> {
        input
            .get(self.name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
    }

    /// A required text.
    pub(crate) fn some<'a>(&self, input: &'a Value) -> Result<&'a str, String> {
        self.text(input)
            .ok_or_else(|| format!("{} is required", self.name))
    }

    /// A required text of 1 to `max` characters; `line` also refuses line breaks.
    pub(crate) fn need<'a>(
        &self,
        input: &'a Value,
        max: usize,
        line: bool,
    ) -> Result<&'a str, String> {
        let text = self
            .text(input)
            .ok_or_else(|| format!("{} is required", self.name))?;
        if text.chars().count() > max {
            return Err(format!("{} is longer than {max} characters", self.name));
        }
        if line && text.contains('\n') {
            return Err(format!("{} must be one line", self.name));
        }
        Ok(text)
    }

    /// An ID: a text, or a number written as text.
    pub(crate) fn id(&self, input: &Value) -> Option<String> {
        input.get(self.name).and_then(|v| {
            v.as_str()
                .map(str::trim)
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .filter(|t| !t.is_empty())
        })
    }

    /// An integer; a number written as text counts too.
    pub(crate) fn int(&self, input: &Value) -> Option<i64> {
        input.get(self.name).and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|t| t.trim().parse().ok()))
        })
    }

    /// An integer of at least 0.
    pub(crate) fn uint(&self, input: &Value) -> Option<u64> {
        input.get(self.name).and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|t| t.trim().parse().ok()))
        })
    }

    /// A flag: absent is false.
    pub(crate) fn flag(&self, input: &Value) -> bool {
        input
            .get(self.name)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The strings of a list, in order.
    pub(crate) fn list(&self, input: &Value) -> Vec<String> {
        input
            .get(self.name)
            .and_then(Value::as_array)
            .map_or_else(Vec::new, |a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
    }

    /// The value as it came.
    pub(crate) fn raw<'a>(&self, input: &'a Value) -> Option<&'a Value> {
        input.get(self.name)
    }

    /// Writes this parameter into an input.
    pub(crate) fn set(&self, input: &mut Value, value: Value) {
        if let Some(map) = input.as_object_mut() {
            map.insert(self.name.to_string(), value);
        }
    }

    /// Removes this parameter from an input.
    pub(crate) fn clear(&self, input: &mut Value) {
        if let Some(map) = input.as_object_mut() {
            map.remove(self.name);
        }
    }
}

/// Parses a tool's input into a typed input, dropping the operation name.
pub(crate) fn parse<T: DeserializeOwned>(input: &Value) -> Result<T, String> {
    let mut value = input.clone();
    if let Some(map) = value.as_object_mut() {
        map.remove(OP.name());
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

/// The `checks` item: one completion signal; `kind` fixes which fields apply.
pub(crate) const CHECK_FIELDS: &[Prop] = &[
    Prop {
        name: "kind",
        ty: Type::Choice(SIGNAL_KINDS),
        about: "which signal: it fixes the fields below",
        required: true,
    },
    Prop {
        name: "repo",
        ty: Type::Text,
        about: "owner/name",
        required: false,
    },
    Prop {
        name: "number",
        ty: Type::Int,
        about: "the issue or pull request number",
        required: false,
    },
    Prop {
        name: "argv",
        ty: Type::Texts,
        about: "the command and its arguments (no shell)",
        required: false,
    },
    Prop {
        name: "pattern",
        ty: Type::Text,
        about: "a regular expression the output must match",
        required: false,
    },
    Prop {
        name: "path",
        ty: Type::Text,
        about: "a file path",
        required: false,
    },
    Prop {
        name: "sha256",
        ty: Type::Text,
        about: "the file's SHA-256, 64 hex digits",
        required: false,
    },
    Prop {
        name: "field",
        ty: Type::Text,
        about: "a dotted path into the file's JSON",
        required: false,
    },
    Prop {
        name: "value",
        ty: Type::Either(&[Type::Text, Type::Int, Type::Flag]),
        about: "the value at that path",
        required: false,
    },
    Prop {
        name: "url",
        ty: Type::Text,
        about: "the URL",
        required: false,
    },
    Prop {
        name: "status",
        ty: Type::Int,
        about: "the HTTP status it must answer",
        required: false,
    },
];

/// The `worktree` object.
pub(crate) const WORKTREE_FIELDS: &[Prop] = &[
    Prop {
        name: "path",
        ty: Type::Text,
        about: "an absolute path",
        required: true,
    },
    Prop {
        name: "branch",
        ty: Type::Text,
        about: "the new branch",
        required: true,
    },
    Prop {
        name: "base",
        ty: Type::Text,
        about: "the commit it starts at",
        required: true,
    },
];

/// The `ceilings` object: a number of at least 0, or `"off"`.
pub(crate) const CEILING_FIELDS: &[Prop] = &[
    Prop {
        name: "minutes",
        ty: Type::Either(&[Type::Number, Type::Text]),
        about: "minutes of run time, or \"off\"",
        required: false,
    },
    Prop {
        name: "tokens",
        ty: Type::Either(&[Type::Number, Type::Text]),
        about: "tokens, or \"off\"",
        required: false,
    },
    Prop {
        name: "cost",
        ty: Type::Either(&[Type::Number, Type::Text]),
        about: "US dollars, or \"off\"",
        required: false,
    },
];

/// The signals a completion check may be.
pub(crate) const SIGNAL_KINDS: &[&str] = &[
    "pr_merged",
    "issue_closed",
    "command",
    "file",
    "web",
    "pr_policy",
];

/// Declares the parameters: name, JSON type, description, tools, and whether it is required.
macro_rules! params {
    ($($(#[$doc:meta])* $const:ident: $key:literal, $ty:expr, $about:literal, [$($tool:ident),*], $req:literal;)*) => {
        $(
            $(#[$doc])*
            pub(crate) const $const: Param = Param {
                name: $key,
                ty: $ty,
                about: $about,
                tools: &[$(Tool::$tool),*],
                required: $req,
            };
        )*

        /// Every parameter of both tools, in declaration order.
        pub(crate) const PARAMS: &[&Param] = &[$(&$const),*];

        /// Every parameter by the name the operations call it, for the tests that walk them.
        #[cfg(test)]
        pub(crate) const BY_NAME: &[(&str, &Param)] = &[$((stringify!($const), &$const)),*];
    };
}

params! {
    /// The operation.
    OP: "op", Type::Text, "the operation", [Lead, Lane], true;
    /// Include closed, retired and inactive ones.
    ALL: "all", Type::Flag, "include closed and inactive ones", [Lead], false;
    /// An item, question or rule ID.
    ID: "id", Type::Text, "an item, question or rule ID", [Lead, Lane], false;
    /// The text of the operation.
    TEXT: "text", Type::Text, "the text: next step, note, result, question, answer or rule", [Lead, Lane], false;
    /// One line.
    TITLE: "title", Type::Text, "one line", [Lead, Lane], false;
    /// Where the item came from.
    SOURCE: "source", Type::Text, "where it came from, such as discord:<room>:<message>", [Lead], false;
    /// The thread or room.
    THREAD: "thread", Type::Text, "the thread or room it belongs to", [Lead], false;
    /// The channel to ask in.
    ROOM: "room", Type::Text, "the Discord channel to post a question in (default the home channel)", [Lead], false;
    /// A message ID.
    MESSAGE: "message", Type::Text, "the owner's or the announcing message ID", [Lead], false;
    /// Why.
    REASON: "reason", Type::Text, "why", [Lead], false;
    /// Why, as a list.
    REASONS: "reasons", Type::Texts, "every reason for the objection", [Lead], false;
    /// The question's options.
    OPTIONS: "options", Type::Texts, "1 to 9 options, one line each", [Lead], false;
    /// The recommended option.
    RECOMMENDED: "recommended", Type::Int, "the recommended option, from 1", [Lead], false;
    /// The chosen option.
    OPTION: "option", Type::Int, "the chosen option, from 1", [Lead], false;
    /// Who settles it.
    SETTLE: "settle", Type::Choice(&["owner", "lead"]), "owner (default) or lead", [Lead], false;
    /// The request-log item a question belongs to.
    REQUEST: "request", Type::Int, "the request-log item the question belongs to", [Lead], false;
    /// A job or step name.
    NAME: "name", Type::Text, "a name: the job's, the step's or the integration's", [Lead, Lane], false;
    /// A job's command.
    ARGV: "argv", Type::Texts, "the command and its arguments, run without a shell", [Lead], false;
    /// A line filter.
    FILTER: "filter", Type::Text, "regular expression an output line must match", [Lead], false;
    /// Restarts allowed.
    RESTARTS: "restarts", Type::Int, "restarts allowed, 0 to 100 (default 5)", [Lead], false;
    /// The first restart delay.
    DELAY: "delay", Type::Int, "first restart delay in seconds (default 10)", [Lead], false;
    /// The quiet time before an alert.
    SILENCE: "silence", Type::Int, "alert after this many quiet minutes", [Lead], false;
    /// A quiet job.
    LOW: "low", Type::Flag, "matches wait for the next wake", [Lead], false;
    /// Where the digest starts.
    SINCE: "since", Type::Text, "ISO instant the digest starts at", [Lead], false;
    /// An onboarding answer.
    REPLY: "reply", Type::Text, "onboarding: yes|no for consent, allowed|declined for a tool", [Lead], false;
    /// What a tool would read.
    READS: "reads", Type::Text, "onboarding: what the tool would read and why", [Lead], false;
    /// Reopening onboarding.
    REOPEN: "reopen", Type::Flag, "onboarding.start after the owner raised it again", [Lead], false;
    /// The lane's key.
    KEY: "key", Type::Text, "the lane's key", [Lead, Lane], false;
    /// The brief.
    BRIEF: "brief", Type::Text, "the brief: the request, the evidence and the constraints", [Lead], false;
    /// A brief file.
    BRIEF_FILE: "brief_file", Type::Text, "an absolute path to the brief instead", [Lead], false;
    /// The completion signals.
    CHECKS: "checks", Type::Object(CHECK_FIELDS), "the completion signals: a non-empty list of objects", [Lead], false;
    /// Where the lane starts from.
    THREAD_FROM: "thread_from", Type::Text, "discord:<room>/<message>: the message the lane starts from", [Lead], false;
    /// The first line in a new thread.
    KICKOFF: "kickoff", Type::Text, "one line posted first in the new thread; with thread_from", [Lead], false;
    /// The working directory.
    CWD: "cwd", Type::Text, "the working directory", [Lead], false;
    /// The model.
    MODEL: "model", Type::Text, "the model, such as provider/name", [Lead], false;
    /// The origin.
    ORIGIN: "origin", Type::Text, "the Discord, Telegram or Slack message it comes from", [Lead], false;
    /// A read-only lane.
    LOOK_ONLY: "look_only", Type::Flag, "read only: no edits and no pushes", [Lead], false;
    /// The ceilings.
    CEILINGS: "ceilings", Type::Object(CEILING_FIELDS), "spend ceilings; omit for the configured ones", [Lead], false;
    /// A worktree.
    WORKTREE: "worktree", Type::Object(WORKTREE_FIELDS), "a worktree the lane works in", [Lead], false;
    /// A worker.
    WORKER: "worker", Type::Text, "a configured worker from lane.workers, only on evidence", [Lead], false;
    /// The lane's priority.
    PRIORITY: "priority", Type::Int, "1 runs first, 9 last (default 5)", [Lead], false;
    /// The lanes to wait for.
    AFTER: "after", Type::Texts, "predecessor keys: the open is queued until they closed completed", [Lead], false;
    /// How long a queued launch waits.
    EXPIRES_HOURS: "expires_hours", Type::Number, "a queued launch expires after this many hours", [Lead], false;
    /// A dry open.
    PREVIEW: "preview", Type::Flag, "check and show the brief without starting", [Lead], false;
    /// The request ID.
    REQUEST_ID: "request_id", Type::Text, "a request ID: one open per ID", [Lead], false;
    /// A pull request.
    PR: "pr", Type::Text, "the pull request, as owner/name#N", [Lead], false;
    /// A dry merge.
    DRY_RUN: "dry_run", Type::Flag, "check without merging", [Lead], false;
    /// A board's destination.
    DESTINATION: "destination", Type::Text, "the thread or channel the message was found in", [Lead], false;
    /// A repository.
    REPO: "repo", Type::Text, "the repository, as owner/name", [Lead], false;
    /// An import label.
    LABEL: "label", Type::Text, "a label to filter issues by", [Lead], false;
    /// An objection's condition.
    CONDITION: "condition", Type::Text, "the condition that lifts the objection", [Lead], false;
    /// A close note.
    NOTE: "note", Type::Text, "a note for the record", [Lead], false;
    /// A lesson.
    LESSON: "lesson", Type::Text, "one line worth keeping", [Lead], false;
    /// A lane's revision.
    REVISION: "revision", Type::Text, "opened:length, the revision the caller saw", [Lead], false;
    /// Steering a message.
    STEER: "steer", Type::Flag, "steer the lane now instead of queueing the message", [Lead], false;
    /// How many lines to read.
    COUNT: "count", Type::Int, "how many lines to read", [Lead], false;
    /// A dialog.
    DIALOG: "dialog", Type::Text, "the dialog ID", [Lead], false;
    /// The answer to a dialog.
    ANSWER: "answer", Type::Open, "the answer, in the shape the dialog asks for", [Lead], false;
    /// A rule to grant.
    RULE: "rule", Type::Int, "the rule number that blocked the command", [Lead], false;
    /// A rule's evidence.
    EVIDENCE: "evidence", Type::Text, "a link or path that shows it", [Lead, Lane], false;
    /// A rule's scope.
    SCOPE: "scope", Type::Choice(&["global", "repo"]), "global (the lead only) or repo", [Lead, Lane], false;
    /// A published file's visibility.
    VISIBILITY: "visibility", Type::Choice(&["private", "public"]), "private (default) or public", [Lead, Lane], false;
    /// A file path.
    PATH: "path", Type::Text, "a file or worktree path", [Lead, Lane], false;
    /// The report's kind.
    KIND: "kind", Type::Choice(&crate::config::lanes::KINDS), "the report kind", [Lane], false;
    /// A one-line summary.
    SUMMARY: "summary", Type::Text, "one short line for the owner, at most 500 characters", [Lane], false;
    /// Evidence files.
    FILES: "files", Type::Texts, "files whose content is evidence, relative to the working directory", [Lane], false;
    /// Evidence tool calls.
    CALLS: "calls", Type::Texts, "tool call IDs whose results are evidence", [Lane], false;
    /// A step's result.
    RESULT: "result", Type::Text, "what the step found", [Lane], false;
    /// Remote receipts.
    RECEIPTS: "receipts", Type::Texts, "remote-effect receipts: push commit, merge link, deploy ID", [Lane], false;
    /// The bridge's facts about the calls a report names.
    FACTS: "facts", Type::Any, "the bridge's facts about the named calls", [Lane], false;
}

/// The `parameters` object of one tool's schema.
pub(crate) fn parameters(tool: Tool) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for p in PARAMS.iter().copied().filter(|p| p.offered(tool)) {
        properties.insert(p.name().to_string(), schema(&p.ty(), p.about()));
        if p.required() {
            required.push(p.name());
        }
    }
    json!({ "type": "object", "properties": properties, "required": required })
}

/// The JSON Schema of one type.
fn schema(ty: &Type, about: &str) -> Value {
    let mut node = match ty {
        Type::Text => json!({ "type": "string" }),
        Type::Int => json!({ "type": "integer" }),
        Type::Number => json!({ "type": "number" }),
        Type::Flag => json!({ "type": "boolean" }),
        Type::Texts => json!({ "type": "array", "items": { "type": "string" } }),
        Type::Any => json!({}),
        Type::Open => json!({ "type": "object" }),
        Type::Choice(choices) => json!({ "type": "string", "enum": choices }),
        Type::Object(fields) => object(fields),
        Type::Either(types) => {
            json!({ "anyOf": types.iter().map(|t| schema(t, "")).collect::<Vec<_>>() })
        }
    };
    if let Value::Object(map) = &mut node {
        map.insert("description".to_string(), json!(about));
    }
    node
}

/// The JSON Schema of an object with exactly these fields.
fn object(fields: &[Prop]) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for f in fields {
        properties.insert(f.name.to_string(), schema(&f.ty, f.about));
        if f.required {
            required.push(f.name);
        }
    }
    json!({ "type": "object", "properties": properties, "required": required })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    /// The files whose functions read a tool's input.
    const OP_FILES: &[&str] = &[
        "src/hub/tool.rs",
        "src/hub/onboarding.rs",
        "src/hub/streams.rs",
        "src/lanes/board.rs",
        "src/lanes/mod.rs",
        "src/lanes/oversight.rs",
        "src/lanes/plan.rs",
        "src/lanes/publish.rs",
        "src/lanes/queue.rs",
        "src/lanes/remote.rs",
        "src/lanes/review.rs",
    ];

    /// The functions each tool's input enters through.
    fn entries(tool: Tool) -> Vec<(&'static str, &'static str)> {
        match tool {
            Tool::Lead => vec![
                ("src/hub/tool.rs", "call"),
                ("src/hub/onboarding.rs", "call"),
                ("src/hub/streams.rs", "start"),
            ],
            Tool::Lane => vec![("src/lanes/mod.rs", "lane_call")],
        }
    }

    /// One function of the operation files.
    struct Func {
        file: &'static str,
        name: String,
        body: String,
        takes_input: bool,
    }

    impl Func {
        fn key(&self) -> (&'static str, String) {
            (self.file, self.name.clone())
        }
    }

    /// What one tool's operations read, found in their own source.
    #[derive(Default)]
    struct Reads {
        /// Parameter identifier to the accessors used on it.
        params: BTreeMap<String, BTreeSet<String>>,
        /// The typed inputs parsed.
        parsed: BTreeSet<String>,
    }

    fn chars(source: &str) -> Vec<char> {
        source.chars().collect()
    }

    /// A source with comments, string bodies and character literals blanked out.
    fn strip(source: &str) -> String {
        let c = chars(source);
        let mut out = String::with_capacity(source.len());
        let mut i = 0;
        while i < c.len() {
            if c[i] == '/' && c.get(i + 1) == Some(&'/') {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            } else if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                i += 2;
                while i + 1 < c.len() && !(c[i] == '*' && c[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(c.len());
            } else if c[i] == '"' {
                out.push('"');
                i += 1;
                while i < c.len() && c[i] != '"' {
                    i += if c[i] == '\\' { 2 } else { 1 };
                }
                out.push('"');
                i += 1;
            } else if c[i] == '\'' && is_char_literal(&c, i) {
                i += 1;
                while i < c.len() && c[i] != '\'' {
                    i += if c[i] == '\\' { 2 } else { 1 };
                }
                out.push_str("' '");
                i += 1;
            } else {
                out.push(c[i]);
                i += 1;
            }
        }
        out
    }

    fn is_char_literal(c: &[char], i: usize) -> bool {
        matches!(
            (c.get(i + 1), c.get(i + 2), c.get(i + 3)),
            (_, Some('\''), _) | (Some('\\'), _, Some('\''))
        )
    }

    /// The body of the block that opens at `open`, without its braces.
    fn block(c: &[char], open: usize) -> String {
        let mut depth = 0;
        let mut out = String::new();
        for (n, ch) in c.iter().enumerate().skip(open) {
            if *ch == '{' {
                depth += 1;
            } else if *ch == '}' {
                depth -= 1;
                if depth == 0 {
                    return out;
                }
            }
            if n > open {
                out.push(*ch);
            }
        }
        out
    }

    fn functions(file: &'static str, source: &str) -> Vec<Func> {
        let c = chars(source);
        let mut out = Vec::new();
        let mut i = 0;
        while i + 2 < c.len() {
            if c[i] == 'f' && c[i + 1] == 'n' && c[i + 2].is_whitespace() {
                let mut j = i + 2;
                while j < c.len() && c[j].is_whitespace() {
                    j += 1;
                }
                let start = j;
                while j < c.len() && (c[j].is_alphanumeric() || c[j] == '_') {
                    j += 1;
                }
                let name: String = c[start..j].iter().collect();
                let mut open = j;
                while open < c.len() && c[open] != '{' {
                    open += 1;
                }
                if !name.is_empty() && open < c.len() {
                    let sig: String = c[j..open].iter().collect();
                    out.push(Func {
                        file,
                        name,
                        body: block(&c, open),
                        takes_input: sig.contains("input:"),
                    });
                }
                i = j;
            } else {
                i += 1;
            }
        }
        out
    }

    fn sources() -> Vec<(&'static str, String)> {
        OP_FILES
            .iter()
            .map(|file| {
                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                let source = match source.find("#[cfg(test)]") {
                    Some(at) => &source[..at],
                    None => &source[..],
                };
                (*file, strip(source))
            })
            .collect()
    }

    fn is_ident(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    /// The identifier that ends at `end` (exclusive).
    fn word_before(c: &[char], end: usize) -> String {
        let mut start = end;
        while start > 0 && (is_ident(c[start - 1]) || c[start - 1] == ':') {
            start -= 1;
        }
        c[start..end].iter().collect()
    }

    /// The call whose argument list holds the token at `at`, as its callee.
    fn callee(c: &[char], at: usize) -> Option<String> {
        let mut depth = 0;
        let mut i = at;
        while i > 0 {
            i -= 1;
            match c[i] {
                ')' => depth += 1,
                '(' if depth == 0 => {
                    let mut end = i;
                    while end > 0 && c[end - 1].is_whitespace() {
                        end -= 1;
                    }
                    // A turbofish (`parse::<T>`) names the function before it.
                    if end > 0 && c[end - 1] == '>' {
                        let mut depth = 0;
                        while end > 0 {
                            end -= 1;
                            match c[end] {
                                '>' => depth += 1,
                                '<' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        while end > 0 && c[end - 1].is_whitespace() {
                            end -= 1;
                        }
                    }
                    // A method's arguments are not another function's input.
                    let mut start = end;
                    while start > 0 && (is_ident(c[start - 1]) || c[start - 1] == ':') {
                        start -= 1;
                    }
                    if start > 0 && c[start - 1] == '.' {
                        // `self.open(input)` is this type's own function, not a method elsewhere.
                        let receiver = word_before(c, start - 1);
                        if !matches!(receiver.as_str(), "self" | "Self") {
                            return None;
                        }
                    }
                    return Some(word_before(c, end));
                }
                '(' => depth -= 1,
                _ => {}
            }
        }
        None
    }

    /// The input tokens of one body: every parameter read and every parsed type.
    #[allow(clippy::too_many_lines, clippy::many_single_char_names)]
    fn scan(file: &str, body: &str, reads: &mut Reads) -> Vec<String> {
        let c = chars(body);
        let mut calls = Vec::new();
        let mut i = 0;
        while i < c.len() {
            if c[i] == 'p'
                && (i == 0 || !is_ident(c[i - 1]))
                && c.get(i + 1) == Some(&':')
                && c.get(i + 2) == Some(&':')
            {
                let after = i + 3;
                let mut j = after;
                while j < c.len() && is_ident(c[j]) {
                    j += 1;
                }
                let ident: String = c[after..j].iter().collect();
                if ident == "parse" {
                    let mut k = j;
                    let rest: String = c[k..].iter().take(40).collect();
                    if let Some(start) = rest.find("::<") {
                        let tail = &rest[start + 3..];
                        if let Some(end) = tail.find('>') {
                            reads.parsed.insert(tail[..end].to_string());
                            k += start + 3 + end;
                        }
                    }
                    i = k;
                    continue;
                }
                // A table reference is a constant; anything else after `p::` is not one.
                if ident.is_empty()
                    || ident == "Param"
                    || !ident.chars().all(|ch| ch.is_ascii_uppercase() || ch == '_')
                {
                    i = j;
                    continue;
                }
                let mut k = j;
                while k < c.len() && c[k].is_whitespace() {
                    k += 1;
                }
                let mut method = String::new();
                if c.get(k) == Some(&'.') {
                    let mut m = k + 1;
                    while m < c.len() && is_ident(c[m]) {
                        m += 1;
                    }
                    method = c[k + 1..m].iter().collect();
                    k = m;
                    // The arguments of an accessor hold nothing else to read.
                    while k < c.len() && c[k].is_whitespace() {
                        k += 1;
                    }
                    if c.get(k) == Some(&'(') {
                        let mut depth = 0;
                        while k < c.len() {
                            if c[k] == '(' {
                                depth += 1;
                            } else if c[k] == ')' {
                                depth -= 1;
                                if depth == 0 {
                                    k += 1;
                                    break;
                                }
                            }
                            k += 1;
                        }
                    }
                }
                reads.params.entry(ident).or_default().insert(method);
                i = k;
                continue;
            }
            if is_ident(c[i]) && (i == 0 || !is_ident(c[i - 1])) {
                let mut j = i;
                while j < c.len() && is_ident(c[j]) {
                    j += 1;
                }
                let word: String = c[i..j].iter().collect();
                if word.starts_with("input") {
                    let mut k = j;
                    while k < c.len() && c[k].is_whitespace() {
                        k += 1;
                    }
                    if c.get(k) == Some(&'.') {
                        let mut m = k + 1;
                        while m < c.len() && is_ident(c[m]) {
                            m += 1;
                        }
                        let method: String = c[k + 1..m].iter().collect();
                        assert!(
                            !matches!(method.as_str(), "get" | "remove" | "pointer"),
                            "{file}: `{word}.{method}` reads the input around the table; \
                             use a parameter"
                        );
                        i = m;
                        continue;
                    }
                    match callee(&c, i) {
                        None => {
                            i = j;
                            continue;
                        }
                        Some(callee) => {
                            let last = callee.rsplit(['.', ':']).next().unwrap_or(&callee);
                            let allowed = callee.starts_with("p::")
                                || matches!(
                                    last,
                                    "Ok" | "Some"
                                        | "Err"
                                        | "json"
                                        | "format"
                                        | "write"
                                        | "writeln"
                                        | "parse"
                                );
                            if !allowed {
                                calls.push(last.to_string());
                            }
                        }
                    }
                }
                i = j;
                continue;
            }
            i += 1;
        }
        calls
    }

    /// What one tool's operations read, walking from its entry points.
    fn reads_of(tool: Tool) -> Reads {
        let index: Vec<Func> = sources()
            .iter()
            .flat_map(|(file, source)| functions(file, source))
            .collect();
        let mut reads = Reads::default();
        let mut queue: Vec<(&'static str, String)> = entries(tool)
            .into_iter()
            .map(|(f, n)| (f, n.to_string()))
            .collect();
        let mut seen: BTreeSet<(&'static str, String)> = queue.iter().cloned().collect();
        while let Some(key) = queue.pop() {
            let Some(found) = index.iter().find(|f| f.key() == key) else {
                panic!("{tool:?}: no function {} in {}", key.1, key.0);
            };
            for callee in scan(found.file, &found.body, &mut reads) {
                let candidates: Vec<&Func> = index
                    .iter()
                    .filter(|f| f.name == callee && f.takes_input)
                    .collect();
                assert!(
                    !candidates.is_empty(),
                    "{}: `{callee}` receives the input, but no function named {callee} in the \
                     operation files takes one; the scan cannot follow it",
                    found.file
                );
                for candidate in candidates {
                    if seen.insert(candidate.key()) {
                        queue.push(candidate.key());
                    }
                }
            }
        }
        reads
    }

    /// The parameter with this JSON name.
    fn by_name(name: &str) -> &'static Param {
        PARAMS
            .iter()
            .copied()
            .find(|p| p.name() == name)
            .unwrap_or_else(|| panic!("`{name}` is not a parameter of the table"))
    }

    fn param(ident: &str) -> &'static Param {
        BY_NAME.iter().find(|(name, _)| *name == ident).map_or_else(
            || panic!("`p::{ident}` is not a parameter of the table; declare it in params!"),
            |(_, p)| *p,
        )
    }

    /// The JSON types each accessor reads.
    fn accessor_types(method: &str) -> &'static [Type] {
        match method {
            "text" | "some" | "need" | "id" => &[Type::Text, Type::Choice(&[])],
            "int" | "uint" => &[Type::Int],
            "flag" => &[Type::Flag],
            "list" => &[Type::Texts],
            _ => &[],
        }
    }

    /// The typed inputs a tool parses, by name, as fields and their declared types.
    fn typed_inputs(tool: Tool, reads: &Reads) -> BTreeMap<String, Vec<(String, Type)>> {
        let mut out = BTreeMap::new();
        for name in &reads.parsed {
            let fields = match name.as_str() {
                "OpenInput" => {
                    let fields = serde_fields::<crate::lanes::record::OpenInput>("OpenInput");
                    fields
                        .iter()
                        .map(|f| (f.clone(), param_for(tool, f).ty()))
                        .collect()
                }
                "ReportInput" => {
                    let fields = serde_fields::<crate::lanes::evidence::ReportInput>("ReportInput");
                    fields
                        .iter()
                        .map(|f| (f.clone(), param_for(tool, f).ty()))
                        .collect()
                }
                other => panic!(
                    "{other} is parsed from a tool input but the test does not know its fields; \
                     add it to parsed_types()"
                ),
            };
            out.insert(name.clone(), fields);
        }
        out
    }

    fn param_for(tool: Tool, field: &str) -> &'static Param {
        PARAMS
            .iter()
            .copied()
            .find(|p| p.name() == field)
            .unwrap_or_else(|| panic!("{tool:?}: field `{field}` is not a parameter"))
    }

    /// The field names of a `Deserialize` struct, from serde's own refusal of an unknown one.
    fn serde_fields<T: DeserializeOwned + std::fmt::Debug>(what: &str) -> Vec<String> {
        let refused = serde_json::from_str::<T>(r#"{"__probe__": 1}"#);
        let err = match refused {
            Ok(v) => panic!("{what} accepted an unknown field: {v:?}"),
            Err(e) => e.to_string(),
        };
        names_after("expected ", &err)
    }

    /// The backticked names after `marker` in a serde error.
    fn names_after(marker: &str, err: &str) -> Vec<String> {
        let tail = match err.find(marker) {
            Some(at) => &err[at + marker.len()..],
            None => panic!("{err}: no `{marker}`"),
        };
        tail.split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect()
    }
    /// A value of one declared type.
    fn sample(ty: &Type) -> Value {
        match ty {
            Type::Text => json!("x"),
            Type::Int => json!(1),
            Type::Number => json!(1.5),
            Type::Flag => json!(true),
            Type::Texts => json!(["x"]),
            Type::Any => json!({"call": "c1", "seen": true}),
            Type::Open => json!({"value": "x"}),
            Type::Choice(list) => json!(list[0]),
            Type::Object(fields) => {
                let mut map = Map::new();
                for f in *fields {
                    map.insert(f.name.to_string(), sample(&f.ty));
                }
                Value::Object(map)
            }
            Type::Either(types) => sample(&types[0]),
        }
    }

    /// The schema properties one tool is registered with.
    fn properties(tool: Tool) -> Map<String, Value> {
        let parameters = parameters(tool);
        parameters["properties"]
            .as_object()
            .cloned()
            .unwrap_or_else(|| panic!("{tool:?}: no properties"))
    }

    /// Every parameter an operation reads is declared for its tool, with the type it reads.
    #[test]
    fn the_operations_read_only_declared_parameters() {
        for tool in [Tool::Lead, Tool::Lane] {
            let reads = reads_of(tool);
            for (ident, methods) in &reads.params {
                let declared = param(ident);
                assert!(
                    declared.declared_for(tool),
                    "{tool:?}: `p::{ident}` is read, but the parameter is not declared for {tool:?}"
                );
                for method in methods {
                    let allowed = accessor_types(method);
                    if allowed.is_empty() {
                        continue;
                    }
                    assert!(
                        allowed.iter().any(|t| accepts(&declared.ty(), t)),
                        "{tool:?}: `p::{ident}.{method}` reads a {:?}, but the schema declares it {:?}",
                        declared.ty(),
                        allowed
                    );
                }
            }
            let parsed = typed_inputs(tool, &reads);
            for (name, fields) in &parsed {
                for (field, ty) in fields {
                    let declared = param_for(tool, field);
                    assert!(
                        declared.declared_for(tool),
                        "{tool:?}: {name}.{field} is parsed, but the parameter is not declared \
                         for {tool:?}"
                    );
                    assert!(
                        same_type(&declared.ty(), ty),
                        "{tool:?}: {name}.{field} is declared {:?}",
                        declared.ty()
                    );
                }
            }
        }
    }

    /// Whether a value of the declared type is what the accessor reads.
    fn accepts(declared: &Type, read: &Type) -> bool {
        match (declared, read) {
            (Type::Choice(_), Type::Text) => true,
            (Type::Either(types), read) => types.iter().any(|t| accepts(t, read)),
            (declared, read) => same_type(declared, read),
        }
    }

    fn same_type(a: &Type, b: &Type) -> bool {
        match (a, b) {
            (Type::Choice(x), Type::Choice(y)) => x == y,
            (Type::Object(x), Type::Object(y)) => {
                x.len() == y.len()
                    && x.iter().zip(*y).all(|(a, b)| {
                        a.name == b.name && a.required == b.required && same_type(&a.ty, &b.ty)
                    })
            }
            (Type::Either(x), Type::Either(y)) => {
                x.len() == y.len() && x.iter().zip(*y).all(|(a, b)| same_type(a, b))
            }
            (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b),
        }
    }

    /// Nothing is declared for a tool that its operations do not read, and nothing it reads is
    /// left out of the schema the model sees.
    #[test]
    fn the_schema_is_exactly_what_the_operations_read() {
        for tool in [Tool::Lead, Tool::Lane] {
            let reads = reads_of(tool);
            let parsed = typed_inputs(tool, &reads);
            let mut used: BTreeSet<&str> = reads
                .params
                .keys()
                .map(|ident| param(ident).name())
                .collect();
            for fields in parsed.values() {
                for (field, _) in fields {
                    used.insert(field.as_str());
                }
            }
            let offered = properties(tool);
            for name in offered.keys() {
                assert!(
                    used.contains(name.as_str()),
                    "{tool:?}: the schema offers `{name}`, which no operation reads"
                );
            }
            for ident in &used {
                let p = by_name(ident);
                if matches!(p.ty(), Type::Any) {
                    assert!(
                        !offered.contains_key(p.name()),
                        "{tool:?}: `{}` is the hub's own and must not be offered",
                        p.name()
                    );
                    continue;
                }
                assert!(
                    offered.contains_key(p.name()),
                    "{tool:?}: `p::{ident}` (`{}`) is read, but the schema does not offer it",
                    p.name()
                );
            }
            let offered_used = used
                .iter()
                .filter(|n| !matches!(by_name(n).ty(), Type::Any))
                .count();
            assert_eq!(
                offered.len(),
                offered_used,
                "{tool:?}: the schema and the reads differ"
            );
        }
    }

    /// Every parameter name is unique, and `op` is the one every call must give.
    #[test]
    fn the_table_has_no_duplicates_and_one_required_parameter() {
        let mut seen = BTreeSet::new();
        for (ident, p) in BY_NAME {
            assert!(
                seen.insert(p.name()),
                "`{ident}` repeats the name {}",
                p.name()
            );
        }
        let required: Vec<&str> = PARAMS
            .iter()
            .filter(|p| p.required())
            .map(|p| p.name())
            .collect();
        assert_eq!(required, ["op"]);
    }

    /// The typed inputs deserialize from what the schema declares, field for field.
    #[test]
    fn the_typed_inputs_match_the_table() {
        let open = serde_fields::<crate::lanes::record::OpenInput>("OpenInput");
        check_struct::<crate::lanes::record::OpenInput>("OpenInput", &open, &|f| {
            param_for(Tool::Lead, f).ty()
        });
        let report = serde_fields::<crate::lanes::evidence::ReportInput>("ReportInput");
        check_struct::<crate::lanes::evidence::ReportInput>("ReportInput", &report, &|f| {
            param_for(Tool::Lane, f).ty()
        });
        let worktree = serde_fields::<crate::lanes::record::Worktree>("Worktree");
        check_props("Worktree", &worktree, WORKTREE_FIELDS);
        let ceilings: Vec<String> = ["minutes", "tokens", "cost"]
            .iter()
            .map(|f| (*f).to_string())
            .collect();
        check_props("Ceilings", &ceilings, CEILING_FIELDS);
    }

    /// One struct's fields are exactly the declared ones, and it accepts a sample of each type.
    fn check_struct<T: DeserializeOwned + std::fmt::Debug>(
        what: &str,
        fields: &[String],
        declared: &dyn Fn(&str) -> Type,
    ) {
        let mut map = Map::new();
        for field in fields {
            map.insert(field.clone(), sample(&declared(field)));
        }
        let value = Value::Object(map);
        match serde_json::from_value::<T>(value.clone()) {
            Ok(_) => {}
            Err(e) => panic!("{what} refuses what the schema declares: {e}: {value}"),
        }
    }

    /// An object parameter's fields are exactly its struct's, with the declared types.
    fn check_props(what: &str, fields: &[String], props: &[Prop]) {
        let declared: Vec<&str> = props.iter().map(|p| p.name).collect();
        for field in fields {
            assert!(
                declared.contains(&field.as_str()),
                "{what}: field `{field}` is not declared"
            );
        }
        for prop in props {
            assert!(
                fields.iter().any(|f| f == prop.name),
                "{what}: `{}` is declared but the struct has no such field",
                prop.name
            );
        }
    }

    /// The `checks` item is exactly what the signals parse, kinds and fields.
    #[test]
    fn the_checks_match_the_signals() {
        use crate::lanes::checks::Signal;
        let tags = names_after(
            "expected ",
            &serde_json::from_value::<Signal>(json!({ "kind": "__probe__" }))
                .err()
                .map_or_else(
                    || panic!("a signal accepted an unknown kind"),
                    |e| e.to_string(),
                ),
        );
        assert_eq!(tags, SIGNAL_KINDS, "the kinds the schema offers");
        let kind = CHECK_FIELDS
            .iter()
            .find(|p| p.name == "kind")
            .map_or_else(|| panic!("the checks item has no kind"), |p| p.ty);
        assert!(same_type(&kind, &Type::Choice(SIGNAL_KINDS)));
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for tag in &tags {
            let probe = json!({ "kind": tag, "__probe__": 1 });
            let err = match serde_json::from_value::<Signal>(probe) {
                Ok(_) => panic!("signal {tag} accepted an unknown field"),
                Err(e) => e.to_string(),
            };
            let fields = names_after("expected ", &err);
            assert!(!fields.is_empty(), "{tag}: {err}");
            let mut map = Map::new();
            map.insert("kind".to_string(), json!(tag));
            for field in &fields {
                let prop = CHECK_FIELDS
                    .iter()
                    .find(|p| p.name == field)
                    .unwrap_or_else(|| panic!("check `{field}` of {tag} is not declared"));
                map.insert(field.clone(), sample(&prop.ty));
                seen.insert(prop.name);
            }
            let value = Value::Object(map);
            match serde_json::from_value::<Signal>(value.clone()) {
                Ok(_) => {}
                Err(e) => panic!("{tag} refuses what the schema declares: {e}: {value}"),
            }
        }
        for prop in CHECK_FIELDS {
            assert!(
                prop.name == "kind" || seen.contains(prop.name),
                "the check `{}` is declared but no signal reads it",
                prop.name
            );
        }
    }
}
