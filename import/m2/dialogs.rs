//! Pure parsers over OMP rpc-ui frames: goals, models, `get_state`, dialogs (`extension_ui_request`),
//! output entries, `extension_ui_response` answers, and the session status precedence.

use serde::Serialize;
use serde_json::{Map, Value};

use super::events::Events;

fn object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn string(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum GoalStatus {
    None,
    Active,
    Paused,
    Complete,
    Dropped,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Goal {
    pub(super) status: GoalStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) objective: Option<String>,
}

/// OMP's goal readback: `Some(&Value::Null)` is "no goal"; an absent value is unreadable.
pub(super) fn goal_of(value: Option<&Value>) -> Option<Goal> {
    if value.is_some_and(Value::is_null) {
        return Some(Goal {
            status: GoalStatus::None,
            objective: None,
        });
    }
    let goal = object(object(value)?.get("goal"))?;
    let status = match string(goal.get("status"))? {
        "active" => GoalStatus::Active,
        "paused" => GoalStatus::Paused,
        "complete" => GoalStatus::Complete,
        "dropped" => GoalStatus::Dropped,
        "budget-limited" => GoalStatus::Blocked,
        _ => return None,
    };
    let objective = string(goal.get("objective")).map(str::to_owned);
    Some(Goal { status, objective })
}

/// `provider/id` from OMP's model object; a provider containing `/` is ambiguous and unreadable.
pub(super) fn model_of(value: Option<&Value>) -> Option<String> {
    let model = object(value)?;
    let provider = string(model.get("provider"))?;
    let id = string(model.get("id"))?;
    (!provider.is_empty() && !provider.contains('/') && !id.is_empty())
        .then(|| format!("{provider}/{id}"))
}

/// The identity, goal, settlement and model read back from `get_state`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NativeState {
    pub(super) session_id: String,
    pub(super) session_file: String,
    pub(super) goal: Goal,
    pub(super) settled: bool,
    pub(super) foreground: bool,
    pub(super) model: Option<String>,
}

/// Reads a `get_state` reply's data.
pub(super) fn native_state_of(data: Option<&Value>) -> Option<NativeState> {
    let data = object(data)?;
    let session_id = string(data.get("sessionId")).filter(|id| !id.is_empty())?;
    let session_file = string(data.get("sessionFile")).filter(|file| !file.is_empty())?;
    let goal = goal_of(data.get("goal"))?;
    let settled = data.get("isSettled")?.as_bool()?;
    let flag = |key: &str| data.get(key) == Some(&Value::Bool(true));
    Some(NativeState {
        session_id: session_id.to_owned(),
        session_file: session_file.to_owned(),
        goal,
        settled,
        foreground: flag("isStreaming") || flag("isCompacting"),
        model: model_of(data.get("model")),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Question {
    id: String,
    #[serde(rename = "question")]
    text: String,
    options: Vec<String>,
    multi: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum DialogKind {
    Approval,
    Confirm,
    Select,
    Input,
    Editor,
    Ask,
    Unsupported,
}

/// A pending dialog; no kind carries both `options` and `secret`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Dialog {
    pub(super) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    pub(super) kind: DialogKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Vec<String>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    secret: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    questions: Option<Vec<Question>>,
}

impl Dialog {
    const fn bare(id: String, kind: DialogKind) -> Self {
        Self {
            id,
            title: None,
            message: None,
            kind,
            options: None,
            secret: false,
            questions: None,
        }
    }
}

fn question_of(item: &Value) -> Option<Question> {
    let question = item.as_object()?;
    let id = string(question.get("id")).filter(|id| !id.is_empty())?;
    let text = string(question.get("question"))?;
    let options = question
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|choice| string(choice.get("label")).map(str::to_owned))
        .collect();
    Some(Question {
        id: id.to_owned(),
        text: text.to_owned(),
        options,
        multi: question.get("multi") == Some(&Value::Bool(true)),
    })
}

/// Classifies an `extension_ui_request` frame. Unknown methods (including `open_url`) are `unsupported`: they may
/// be gated, but their content is not exposed and they cannot be answered.
pub(super) fn dialog_of(frame: &Value) -> Option<Dialog> {
    let id = string(frame.get("id"))
        .filter(|id| !id.is_empty())?
        .to_owned();
    let summary = |kind| Dialog {
        title: string(frame.get("title")).map(str::to_owned),
        message: string(frame.get("message")).map(str::to_owned),
        ..Dialog::bare(id.clone(), kind)
    };
    match string(frame.get("method")) {
        Some("select") => {
            let options: Vec<String> = frame
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| option.as_str().map(str::to_owned))
                .collect();
            let has = |label: &str| options.iter().any(|option| option == label);
            // OMP presents tool approval as a select with Deny and a single-call approval.
            let approval = has("Deny") && (has("Approve") || has("Approve Once"));
            let kind = if approval {
                DialogKind::Approval
            } else {
                DialogKind::Select
            };
            Some(Dialog {
                options: Some(options),
                ..summary(kind)
            })
        }
        Some("confirm") => Some(summary(DialogKind::Confirm)),
        Some("input") => Some(Dialog {
            secret: frame.get("secret") == Some(&Value::Bool(true)),
            ..summary(DialogKind::Input)
        }),
        Some("editor") => Some(summary(DialogKind::Editor)),
        Some("ask") => {
            let questions: Vec<Question> = frame
                .get("questions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(question_of)
                .collect();
            Some(if questions.is_empty() {
                Dialog::bare(id, DialogKind::Unsupported)
            } else {
                Dialog {
                    questions: Some(questions),
                    ..Dialog::bare(id, DialogKind::Ask)
                }
            })
        }
        Some("cancel" | "notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text") => {
            None
        }
        _ => Some(Dialog::bare(id, DialogKind::Unsupported)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum EntryKind {
    Assistant,
    Output,
    Notice,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Entry {
    kind: EntryKind,
    text: String,
}

const ENTRY_UNITS: usize = 2_000;

/// The last `limit` UTF-16 code units of `text`, never splitting a character.
fn last_units(text: &str, limit: usize) -> &str {
    let mut units = 0;
    for (at, c) in text.char_indices().rev() {
        units += c.len_utf16();
        if units > limit {
            return &text[at + c.len_utf8()..];
        }
    }
    text
}

/// The read-only output ring's view of an assistant message, command output or notice.
pub(super) fn entry_of(frame: &Value) -> Option<Entry> {
    let message = object(frame.get("message"));
    let kind_of = string(frame.get("type"));
    let (kind, text) = if kind_of == Some("message_end")
        && message.is_some_and(|m| string(m.get("role")) == Some("assistant"))
        && let Some(content) = message
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
    {
        let text: String = content
            .iter()
            .filter(|part| string(part.get("type")) == Some("text"))
            .filter_map(|part| string(part.get("text")))
            .collect();
        (EntryKind::Assistant, Some(text))
    } else if kind_of == Some("command_output") {
        (
            EntryKind::Output,
            string(frame.get("text")).map(str::to_owned),
        )
    } else if kind_of == Some("extension_ui_request")
        && string(frame.get("method")) == Some("notify")
    {
        (
            EntryKind::Notice,
            string(frame.get("message")).map(str::to_owned),
        )
    } else {
        return None;
    };
    let text = text.filter(|text| !text.trim().is_empty())?;
    let clean: String = text
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect();
    Some(Entry {
        kind,
        text: last_units(&clean, ENTRY_UNITS).to_owned(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AnswerItem {
    id: String,
    selected_options: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_input: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum AnswerResponse {
    Cancelled(bool),
    Confirmed(bool),
    Value(String),
    Answers(Vec<AnswerItem>),
}

/// An `extension_ui_response` frame: `{type,id,<one response field>}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct AnswerFrame {
    #[serde(rename = "type")]
    kind: &'static str,
    id: String,
    #[serde(flatten)]
    response: AnswerResponse,
}

fn answer_item(question: &Question, answer: &Value) -> Option<AnswerItem> {
    let answer = answer.as_object()?;
    if string(answer.get("id")) != Some(question.id.as_str()) {
        return None;
    }
    let selected = answer.get("selectedOptions")?.as_array()?;
    let selected: Vec<String> = selected
        .iter()
        .map(|option| {
            option
                .as_str()
                .filter(|option| question.options.iter().any(|known| known == option))
                .map(str::to_owned)
        })
        .collect::<Option<_>>()?;
    if selected
        .iter()
        .enumerate()
        .any(|(at, option)| selected[..at].contains(option))
    {
        return None;
    }
    let custom = match answer.get("customInput") {
        None => None,
        Some(Value::String(custom)) => Some(custom.clone()),
        Some(_) => return None,
    };
    let filled = custom.as_deref().is_some_and(|custom| !custom.trim().is_empty());
    if (!question.multi && selected.len() + usize::from(filled) > 1)
        || (selected.is_empty() && !filled)
    {
        return None;
    }
    Some(AnswerItem {
        id: question.id.clone(),
        selected_options: selected,
        custom_input: custom,
    })
}

/// Exactly one recognized answer field for the dialog's kind. Unrecognized fields (notably `timedOut`,
/// which may submit recommended answers) are never forwarded, secrets are never answered, and approvals grant one call
/// only: no session or global grants.
pub(super) fn answer_frame(dialog: &Dialog, answer: &Value) -> Option<AnswerFrame> {
    let response = answer.as_object().filter(|response| response.len() == 1)?;
    let frame = |response| {
        Some(AnswerFrame {
            kind: "extension_ui_response",
            id: dialog.id.clone(),
            response,
        })
    };
    if dialog.kind != DialogKind::Unsupported
        && response.get("cancelled") == Some(&Value::Bool(true))
    {
        return frame(AnswerResponse::Cancelled(true));
    }
    if dialog.secret || dialog.kind == DialogKind::Unsupported {
        return None;
    }
    if dialog.kind == DialogKind::Confirm
        && let Some(Value::Bool(confirmed)) = response.get("confirmed")
    {
        return frame(AnswerResponse::Confirmed(*confirmed));
    }
    let value = string(response.get("value"));
    match dialog.kind {
        DialogKind::Input | DialogKind::Editor => {
            return frame(AnswerResponse::Value(value?.to_owned()));
        }
        DialogKind::Approval | DialogKind::Select => {
            let value = value?;
            if !dialog
                .options
                .as_ref()
                .is_some_and(|options| options.iter().any(|o| o == value))
            {
                return None;
            }
            if dialog.kind == DialogKind::Approval
                && !matches!(value, "Deny" | "Approve Once" | "Approve")
            {
                return None;
            }
            return frame(AnswerResponse::Value(value.to_owned()));
        }
        _ => {}
    }
    let answers = response.get("answers")?.as_array()?;
    let questions = dialog.questions.as_ref()?;
    if answers.len() != questions.len() {
        return None;
    }
    let valid = questions
        .iter()
        .zip(answers)
        .map(|(question, answer)| answer_item(question, answer))
        .collect::<Option<_>>()?;
    frame(AnswerResponse::Answers(valid))
}

/// The `AgentStatus` values a native host reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Error,
    Unknown,
}

/// A session observation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Observation {
    updated_at: String,
    status: AgentStatus,
    held: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    goal: Option<Goal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_since: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    dialogs: Vec<Dialog>,
    recent: Vec<Entry>,
}

/// The session state an observation is computed from.
pub(super) struct Snapshot<'a> {
    /// The OMP child's pid, never the host's.
    pub(super) pid: Option<u32>,
    /// The proven identity.
    pub(super) bound: Option<&'a NativeState>,
    /// The readback this observation answers for (`None` while held or unread).
    pub(super) state: Option<&'a NativeState>,
    /// The last successful readback, which supplies goal and model.
    pub(super) latest: Option<&'a NativeState>,
    pub(super) fault: bool,
    pub(super) stopping: bool,
    pub(super) detail: Option<&'a str>,
    pub(super) events: &'a Events,
}

/// Status precedence is error, blocked, unknown, idle, working; identity fields appear only for a
/// proven, unfaulted session with a readback.
pub(super) fn snapshot(input: &Snapshot<'_>) -> Observation {
    let dialogs = input.events.has_dialogs();
    let held =
        dialogs || input.fault || input.stopping || input.state.is_none() || input.bound.is_none();
    let status = if input.fault {
        AgentStatus::Error
    } else if dialogs
        || input
            .state
            .is_some_and(|state| state.goal.status == GoalStatus::Blocked)
    {
        AgentStatus::Blocked
    } else if input.bound.is_none() || input.stopping {
        AgentStatus::Unknown
    } else {
        match input.state {
            None => AgentStatus::Unknown,
            Some(state) if state.settled => AgentStatus::Idle,
            Some(_) => AgentStatus::Working,
        }
    };
    let identity = input.bound.zip(input.latest).filter(|_| !input.fault);
    Observation {
        updated_at: util::iso(util::now_ms()),
        status,
        held,
        pid: input.pid,
        session_id: identity.map(|(bound, _)| bound.session_id.clone()),
        session_file: identity.map(|(bound, _)| bound.session_file.clone()),
        goal: identity.map(|(_, latest)| latest.goal.clone()),
        model: identity.and_then(|(_, latest)| latest.model.clone()),
        active_since: input.events.active_since(),
        detail: input.detail.map(str::to_owned),
        dialogs: input.events.dialogs().to_vec(),
        recent: input.events.recent().cloned().collect(),
    }
}

/// An optional expected session ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Expected {
    /// Absent: any bound session.
    Unconstrained,
    Session(String),
    /// Present but not a string (including `null`): never the bound session.
    Never,
}

impl Expected {
    fn of(value: Option<&Value>) -> Self {
        match value {
            None => Self::Unconstrained,
            Some(Value::String(session)) => Self::Session(session.clone()),
            Some(_) => Self::Never,
        }
    }

    pub(super) fn admits(&self, bound: &str) -> bool {
        match self {
            Self::Unconstrained => true,
            Self::Session(session) => session == bound,
            Self::Never => false,
        }
    }
}

/// When a local delivery is admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeliverWhen {
    Idle,
    Working,
    Any,
    /// Any other value, which no delivery status equals.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DeliverOptions {
    pub(super) expected_session_id: Expected,
    pub(super) when: DeliverWhen,
}

impl DeliverOptions {
    /// Whether the delivery status (idle, working or blocked) admits this delivery.
    pub(super) fn when_admits(&self, status: AgentStatus) -> bool {
        match self.when {
            DeliverWhen::Any => true,
            DeliverWhen::Idle => status == AgentStatus::Idle,
            DeliverWhen::Working => status == AgentStatus::Working,
            DeliverWhen::Other => false,
        }
    }
}

/// A goal change target. Any value but `Paused` requests `resume`, and only `Paused`/`Active` can be reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GoalTarget {
    Paused,
    Active,
    Other,
}

impl GoalTarget {
    /// The `goal` RPC `op` for this target.
    pub(super) const fn op(self) -> &'static str {
        match self {
            Self::Paused => "pause",
            Self::Active | Self::Other => "resume",
        }
    }

    /// Whether `status` is this target.
    pub(super) fn reached(self, status: GoalStatus) -> bool {
        match self {
            Self::Paused => status == GoalStatus::Paused,
            Self::Active => status == GoalStatus::Active,
            Self::Other => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Delivery {
    Prompt,
    Steer,
}

impl Delivery {
    /// The wire spelling, also the digest prefix `${delivery}\0`.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Steer => "steer",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RemoteInput {
    pub(super) id: String,
    pub(super) text: String,
    pub(super) delivery: Delivery,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ModelSelector {
    pub(super) model: String,
    pub(super) provider: String,
    pub(super) model_id: String,
}

/// A remote command after the session check; `Err` carries the refusal reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RemoteCommand {
    Send(Result<RemoteInput, &'static str>),
    Receipt(Result<String, &'static str>),
    Interrupt,
    Model(Result<ModelSelector, &'static str>),
    Close(Result<bool, &'static str>),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fixture_approval() -> Value {
        json!({
            "type": "extension_ui_request",
            "id": "approval-1",
            "method": "select",
            "title": "Approve this fake write?",
            "options": ["Approve Once", "Approve for this session", "Deny"],
        })
    }

    fn fixture_question() -> Value {
        json!({
            "type": "extension_ui_request",
            "id": "question-1",
            "method": "ask",
            "questions": [{ "id": "single", "question": "Which option?", "options": [{ "label": "A" }, { "label": "B" }] }],
        })
    }

    fn dialog(frame: &Value) -> Dialog {
        dialog_of(frame).expect("dialog")
    }

    fn sent(dialog: &Dialog, answer: &Value) -> Option<String> {
        answer_frame(dialog, answer).map(|frame| serde_json::to_string(&frame).unwrap())
    }

    #[test]
    fn approval_dialog_matches_the_fixture_shape() {
        let approval = dialog(&fixture_approval());
        assert_eq!(
            serde_json::to_string(&approval).unwrap(),
            r#"{"id":"approval-1","title":"Approve this fake write?","kind":"approval","options":["Approve Once","Approve for this session","Deny"]}"#
        );
    }

    #[test]
    fn only_deny_with_a_single_call_approval_is_an_approval() {
        let kind = |options: Value| {
            dialog(&json!({ "id": "d", "method": "select", "options": options })).kind
        };
        assert_eq!(kind(json!(["Approve", "Deny"])), DialogKind::Approval);
        assert_eq!(kind(json!(["Deny", "Approve Once"])), DialogKind::Approval);
        assert_eq!(
            kind(json!(["Approve Once", "Approve for this session"])),
            DialogKind::Select
        );
        assert_eq!(
            kind(json!(["Deny", "Approve for this session"])),
            DialogKind::Select
        );
        assert_eq!(kind(json!(["Yes", "No"])), DialogKind::Select);
        assert_eq!(kind(json!("Deny,Approve")), DialogKind::Select);
        let mixed = dialog(
            &json!({ "id": "d", "method": "select", "options": ["Deny", 1, null, "Approve"] }),
        );
        assert_eq!(mixed.kind, DialogKind::Approval);
        assert_eq!(
            mixed.options,
            Some(vec!["Deny".to_owned(), "Approve".to_owned()])
        );
    }

    #[test]
    fn approvals_answer_one_call_and_never_grant_a_session() {
        let approval = dialog(&fixture_approval());
        assert_eq!(
            sent(&approval, &json!({ "value": "Approve for this session" })),
            None
        );
        assert_eq!(
            sent(&approval, &json!({ "value": "Deny" })).as_deref(),
            Some(r#"{"type":"extension_ui_response","id":"approval-1","value":"Deny"}"#)
        );
        assert!(sent(&approval, &json!({ "value": "Approve Once" })).is_some());
        // Recognized wording that this dialog did not offer.
        assert_eq!(sent(&approval, &json!({ "value": "Approve" })), None);
        let wide = dialog(&json!({
            "id": "w", "method": "select", "options": ["Approve", "Always approve", "Approve for this session", "Deny"],
        }));
        assert!(sent(&wide, &json!({ "value": "Approve" })).is_some());
        assert_eq!(sent(&wide, &json!({ "value": "Always approve" })), None);
        // Plain selects accept any offered option.
        let select = dialog(
            &json!({ "id": "s", "method": "select", "options": ["Always approve", "Other"] }),
        );
        assert!(sent(&select, &json!({ "value": "Always approve" })).is_some());
        assert_eq!(sent(&select, &json!({ "value": "Missing" })), None);
    }

    #[test]
    fn answers_of_the_wrong_kind_or_with_extra_fields_are_refused() {
        let approval = dialog(&fixture_approval());
        for answer in [
            json!({ "value": "Deny", "timedOut": true }),
            json!({ "timedOut": true }),
            json!({}),
            json!({ "confirmed": true }),
            json!({ "value": 1 }),
            json!({ "answers": [] }),
            json!({ "cancelled": false }),
            json!(["Deny"]),
            json!("Deny"),
            Value::Null,
        ] {
            assert_eq!(sent(&approval, &answer), None, "{answer}");
        }
        assert_eq!(
            sent(&approval, &json!({ "cancelled": true })).as_deref(),
            Some(r#"{"type":"extension_ui_response","id":"approval-1","cancelled":true}"#)
        );
        let confirm = dialog(&json!({ "id": "c", "method": "confirm", "title": "Sure?" }));
        assert_eq!(
            sent(&confirm, &json!({ "confirmed": false })).as_deref(),
            Some(r#"{"type":"extension_ui_response","id":"c","confirmed":false}"#)
        );
        assert_eq!(sent(&confirm, &json!({ "confirmed": "yes" })), None);
        assert_eq!(sent(&confirm, &json!({ "value": "yes" })), None);
        let editor = dialog(&json!({ "id": "e", "method": "editor" }));
        assert!(sent(&editor, &json!({ "value": "" })).is_some());
        assert_eq!(sent(&editor, &json!({ "confirmed": true })), None);
        let question = dialog(&fixture_question());
        assert_eq!(sent(&question, &json!({ "value": "B" })), None);
    }

    #[test]
    fn secret_input_is_only_cancellable() {
        let input =
            dialog(&json!({ "id": "i", "method": "input", "title": "Token", "secret": true }));
        assert_eq!(
            serde_json::to_string(&input).unwrap(),
            r#"{"id":"i","title":"Token","kind":"input","secret":true}"#
        );
        assert_eq!(sent(&input, &json!({ "value": "hunter2" })), None);
        assert!(sent(&input, &json!({ "cancelled": true })).is_some());
        let plain = dialog(&json!({ "id": "i", "method": "input", "secret": "yes" }));
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"id":"i","kind":"input"}"#
        );
        assert_eq!(
            sent(&plain, &json!({ "value": "text" })).as_deref(),
            Some(r#"{"type":"extension_ui_response","id":"i","value":"text"}"#)
        );
    }

    #[test]
    fn unknown_ui_is_unsupported_unanswerable_and_hides_content() {
        let url = dialog(
            &json!({ "id": "u", "method": "open_url", "title": "Open", "url": "https://example.test" }),
        );
        assert_eq!(
            serde_json::to_string(&url).unwrap(),
            r#"{"id":"u","kind":"unsupported"}"#
        );
        assert_eq!(sent(&url, &json!({ "cancelled": true })), None);
        assert_eq!(
            sent(&url, &json!({ "value": "https://example.test" })),
            None
        );
        assert_eq!(dialog(&json!({ "id": "m" })).kind, DialogKind::Unsupported);
        assert_eq!(
            dialog(&json!({ "id": "m", "method": 1 })).kind,
            DialogKind::Unsupported
        );
        for method in [
            "cancel",
            "notify",
            "setStatus",
            "setWidget",
            "setTitle",
            "set_editor_text",
        ] {
            assert_eq!(
                dialog_of(&json!({ "id": "n", "method": method })),
                None,
                "{method}"
            );
        }
        assert_eq!(dialog_of(&json!({ "method": "confirm" })), None);
        assert_eq!(dialog_of(&json!({ "id": "", "method": "confirm" })), None);
        assert_eq!(dialog_of(&json!({ "id": 1, "method": "confirm" })), None);
    }

    #[test]
    fn ask_dialogs_keep_only_well_formed_questions() {
        let ask = dialog(&json!({
            "id": "a", "method": "ask", "title": "dropped",
            "questions": [
                { "id": "q", "question": "", "options": [{ "label": "A" }, { "label": 2 }, "B", {}], "multi": true },
                { "id": "", "question": "no id" },
                { "id": "x", "question": 1 },
                "q",
            ],
        }));
        assert_eq!(
            serde_json::to_string(&ask).unwrap(),
            r#"{"id":"a","kind":"ask","questions":[{"id":"q","question":"","options":["A"],"multi":true}]}"#
        );
        let empty = dialog(
            &json!({ "id": "a", "method": "ask", "questions": [{ "id": "", "question": "q" }] }),
        );
        assert_eq!(empty.kind, DialogKind::Unsupported);
        assert_eq!(
            dialog(&json!({ "id": "a", "method": "ask", "questions": {} })).kind,
            DialogKind::Unsupported
        );
    }

    #[test]
    fn ask_answers_must_select_offered_options_per_question() {
        let question = dialog(&fixture_question());
        let one = |answer: Value| sent(&question, &json!({ "answers": [answer] }));
        assert_eq!(
            one(json!({ "id": "single", "selectedOptions": ["wrong"] })),
            None
        );
        assert_eq!(
            one(json!({ "id": "single", "selectedOptions": ["B"] })).as_deref(),
            Some(
                r#"{"type":"extension_ui_response","id":"question-1","answers":[{"id":"single","selectedOptions":["B"]}]}"#
            )
        );
        assert_eq!(
            one(json!({ "id": "single", "selectedOptions": ["A", "B"] })),
            None
        );
        assert_eq!(
            one(json!({ "id": "single", "selectedOptions": ["A"], "customInput": "C" })),
            None
        );
        assert_eq!(
            one(json!({ "id": "single", "selectedOptions": ["A"], "customInput": "  " }))
                .as_deref(),
            Some(
                r#"{"type":"extension_ui_response","id":"question-1","answers":[{"id":"single","selectedOptions":["A"],"customInput":"  "}]}"#
            )
        );
        assert!(
            one(json!({ "id": "single", "selectedOptions": [], "customInput": "C" })).is_some()
        );
        assert_eq!(one(json!({ "id": "single", "selectedOptions": [] })), None);
        assert_eq!(
            one(json!({ "id": "single", "selectedOptions": ["A"], "customInput": null })),
            None
        );
        assert_eq!(
            one(json!({ "id": "other", "selectedOptions": ["A"] })),
            None
        );
        assert_eq!(one(json!({ "selectedOptions": ["A"] })), None);
        assert_eq!(one(json!({ "id": "single", "selectedOptions": "A" })), None);
        assert_eq!(one(json!({ "id": "single", "selectedOptions": [1] })), None);
        assert_eq!(one(json!("single")), None);
        assert_eq!(sent(&question, &json!({ "answers": [] })), None);
        let answer = json!({ "id": "single", "selectedOptions": ["A"] });
        assert_eq!(
            sent(&question, &json!({ "answers": [answer.clone(), answer] })),
            None
        );
        let multi = dialog(&json!({
            "id": "m", "method": "ask",
            "questions": [{ "id": "q", "question": "?", "options": [{ "label": "A" }, { "label": "B" }], "multi": true }],
        }));
        let pick = |answer: Value| sent(&multi, &json!({ "answers": [answer] }));
        assert!(
            pick(json!({ "id": "q", "selectedOptions": ["A", "B"], "customInput": "C" })).is_some()
        );
        assert_eq!(
            pick(json!({ "id": "q", "selectedOptions": ["A", "A"] })),
            None
        );
    }

    #[test]
    fn entries_keep_assistant_text_output_and_notices() {
        let assistant = json!({
            "type": "message_end",
            "message": { "role": "assistant", "content": [
                { "type": "text", "text": "[CATALY message]\n" }, { "type": "toolCall", "text": "skip" },
                { "type": "text", "text": "/only-literal" }, { "type": "text", "text": 1 },
            ] },
        });
        assert_eq!(
            serde_json::to_string(&entry_of(&assistant).unwrap()).unwrap(),
            r#"{"kind":"assistant","text":"[CATALY message]\n/only-literal"}"#
        );
        let user = json!({ "type": "message_end", "message": { "role": "user", "content": [{ "type": "text", "text": "x" }] } });
        assert_eq!(entry_of(&user), None);
        let string_content =
            json!({ "type": "message_end", "message": { "role": "assistant", "content": "x" } });
        assert_eq!(entry_of(&string_content), None);
        let output = entry_of(&json!({ "type": "command_output", "text": "done" })).unwrap();
        assert_eq!(
            (output.kind, output.text.as_str()),
            (EntryKind::Output, "done")
        );
        let notice = entry_of(
            &json!({ "type": "extension_ui_request", "method": "notify", "message": "hi" }),
        )
        .unwrap();
        assert_eq!(notice.kind, EntryKind::Notice);
        assert_eq!(
            entry_of(
                &json!({ "type": "extension_ui_request", "method": "confirm", "message": "hi" })
            ),
            None
        );
        assert_eq!(
            entry_of(&json!({ "type": "command_output", "text": " \n" })),
            None
        );
        assert_eq!(
            entry_of(&json!({ "type": "command_output", "text": 1 })),
            None
        );
        assert_eq!(entry_of(&json!({ "type": "agent_start" })), None);
    }

    #[test]
    fn goals_and_models_read_like_omp_reports_them() {
        let none = Goal {
            status: GoalStatus::None,
            objective: None,
        };
        assert_eq!(goal_of(Some(&Value::Null)), Some(none));
        assert_eq!(goal_of(None), None);
        assert_eq!(goal_of(Some(&json!({}))), None);
        assert_eq!(goal_of(Some(&json!({ "goal": null }))), None);
        assert_eq!(
            goal_of(Some(&json!({ "goal": { "status": "waiting" } }))),
            None
        );
        let blocked = goal_of(Some(
            &json!({ "goal": { "status": "budget-limited", "objective": "o" } }),
        ))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&blocked).unwrap(),
            r#"{"status":"blocked","objective":"o"}"#
        );
        let active = goal_of(Some(
            &json!({ "goal": { "status": "active", "objective": 1 } }),
        ))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&active).unwrap(),
            r#"{"status":"active"}"#
        );
        for status in ["paused", "complete", "dropped"] {
            assert!(goal_of(Some(&json!({ "goal": { "status": status } }))).is_some());
        }
        assert_eq!(
            model_of(Some(&json!({ "provider": "fixture", "id": "initial" }))).as_deref(),
            Some("fixture/initial")
        );
        assert_eq!(
            model_of(Some(&json!({ "provider": "a/b", "id": "c" }))),
            None
        );
        assert_eq!(model_of(Some(&json!({ "provider": "", "id": "c" }))), None);
        assert_eq!(model_of(Some(&json!({ "provider": "a", "id": "" }))), None);
        assert_eq!(model_of(Some(&json!("a/b"))), None);
    }

    fn fixture_state(settled: bool) -> Value {
        json!({
            "sessionId": "fixture-session", "sessionFile": "/fixture/session.jsonl", "isSettled": settled,
            "isStreaming": !settled, "goal": { "goal": { "status": "active", "objective": "o" } },
            "model": { "provider": "fixture", "id": "initial" },
        })
    }

    #[test]
    fn native_state_requires_identity_goal_and_settlement() {
        let state = native_state_of(Some(&fixture_state(false))).unwrap();
        assert_eq!(state.session_id, "fixture-session");
        assert!(state.foreground && !state.settled);
        assert_eq!(state.model.as_deref(), Some("fixture/initial"));
        let mut data = fixture_state(true);
        data["goal"] = Value::Null;
        data["isCompacting"] = json!(true);
        data["model"] = json!({ "provider": "a/b", "id": "c" });
        let state = native_state_of(Some(&data)).unwrap();
        assert_eq!(
            (state.goal.status, state.foreground, state.model),
            (GoalStatus::None, true, None)
        );
        for key in ["sessionId", "sessionFile", "goal", "isSettled"] {
            let mut data = fixture_state(true);
            data.as_object_mut().unwrap().remove(key);
            assert_eq!(native_state_of(Some(&data)), None, "{key}");
        }
        let mut data = fixture_state(true);
        data["isSettled"] = json!(1);
        assert_eq!(native_state_of(Some(&data)), None);
        data["isSettled"] = json!(true);
        data["sessionId"] = json!("");
        assert_eq!(native_state_of(Some(&data)), None);
        assert_eq!(native_state_of(None), None);
    }

    fn base<'a>(events: &'a Events, state: Option<&'a NativeState>) -> Snapshot<'a> {
        Snapshot {
            pid: Some(123),
            bound: state,
            state,
            latest: state,
            fault: false,
            stopping: false,
            detail: None,
            events,
        }
    }

    #[test]
    fn observation_status_and_hold_precedence() {
        let mut events = Events::default();
        let idle = native_state_of(Some(&fixture_state(true))).unwrap();
        let mut blocked = idle.clone();
        blocked.goal.status = GoalStatus::Blocked;
        let observe = |input: &Snapshot<'_>| {
            let observation = snapshot(input);
            (
                observation.status,
                observation.held,
                observation.session_id.is_some(),
            )
        };
        assert_eq!(
            observe(&base(&events, Some(&idle))),
            (AgentStatus::Idle, false, true)
        );
        assert_eq!(
            observe(&base(&events, Some(&blocked))),
            (AgentStatus::Blocked, false, true)
        );
        let faulted = Snapshot {
            fault: true,
            ..base(&events, Some(&blocked))
        };
        assert_eq!(observe(&faulted), (AgentStatus::Error, true, false));
        let stopping = Snapshot {
            stopping: true,
            ..base(&events, Some(&idle))
        };
        assert_eq!(observe(&stopping), (AgentStatus::Unknown, true, true));
        let stopping_blocked = Snapshot {
            stopping: true,
            ..base(&events, Some(&blocked))
        };
        assert_eq!(
            observe(&stopping_blocked),
            (AgentStatus::Blocked, true, true)
        );
        let unread = Snapshot {
            state: None,
            ..base(&events, Some(&idle))
        };
        assert_eq!(observe(&unread), (AgentStatus::Unknown, true, true));
        let unproven = Snapshot {
            bound: None,
            ..base(&events, Some(&idle))
        };
        assert_eq!(observe(&unproven), (AgentStatus::Unknown, true, false));
        let never_read = Snapshot {
            latest: None,
            state: None,
            ..base(&events, Some(&idle))
        };
        assert_eq!(observe(&never_read), (AgentStatus::Unknown, true, false));
        events.record(&fixture_approval());
        let held = Snapshot {
            state: None,
            ..base(&events, Some(&idle))
        };
        assert_eq!(observe(&held), (AgentStatus::Blocked, true, true));
        let faulted_with_dialog = Snapshot {
            fault: true,
            ..base(&events, Some(&idle))
        };
        assert_eq!(
            observe(&faulted_with_dialog),
            (AgentStatus::Error, true, false)
        );
        let observation = snapshot(&base(&events, Some(&idle)));
        assert_eq!(observation.dialogs.len(), 1);
    }
}
