//! Pure parsers over OMP rpc-ui frames (HOST-12 to HOST-14, HOST-18): goal and model readback,
//! `get_state`, dialogs (`extension_ui_request`), output-ring entries, `extension_ui_response` answers,
//! the status precedence, and the spend-offer test of DESIGN 9.2.

use serde::Serialize;
use serde_json::{Map, Value};

fn object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn string(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum GoalStatus {
    None,
    Active,
    Paused,
    Complete,
    Dropped,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Goal {
    pub(crate) status: GoalStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) objective: Option<String>,
}

/// OMP's goal readback: `Some(&Value::Null)` is "no goal"; an absent value is unreadable.
pub(crate) fn goal_of(value: Option<&Value>) -> Option<Goal> {
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
pub(crate) fn model_of(value: Option<&Value>) -> Option<String> {
    let model = object(value)?;
    let provider = string(model.get("provider"))?;
    let id = string(model.get("id"))?;
    (!provider.is_empty() && !provider.contains('/') && !id.is_empty())
        .then(|| format!("{provider}/{id}"))
}

/// Provider exact, model ID case-insensitive (HOST req. 27).
pub(crate) fn same_model(a: &str, b: &str) -> bool {
    match (a.split_once('/'), b.split_once('/')) {
        (Some((pa, ia)), Some((pb, ib))) => pa == pb && ia.eq_ignore_ascii_case(ib),
        _ => false,
    }
}

/// The identity, goal, settlement, model and limit state read back from `get_state`.
// Independent facts OMP reports; none implies another.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadState {
    pub(crate) session_id: String,
    pub(crate) session_file: String,
    pub(crate) goal: Goal,
    pub(crate) settled: bool,
    /// Streaming or compacting.
    pub(crate) foreground: bool,
    pub(crate) streaming: bool,
    pub(crate) queued: u64,
    pub(crate) model: Option<String>,
    pub(crate) thinking: Option<String>,
    /// `usageLimit` says the next request would bill extra usage (DESIGN 9.2).
    pub(crate) extra_usage: bool,
}

impl ReadState {
    /// Idle means no streaming, no compaction, nothing queued and settled (HOST req. 37).
    pub(crate) fn idle(&self) -> bool {
        self.settled && !self.foreground && self.queued == 0
    }
}

/// Reads a `get_state` reply's data.
pub(crate) fn read_state(data: Option<&Value>) -> Option<ReadState> {
    let data = object(data)?;
    let session_id = string(data.get("sessionId")).filter(|id| !id.is_empty())?;
    let session_file = string(data.get("sessionFile")).filter(|file| !file.is_empty())?;
    let goal = goal_of(data.get("goal"))?;
    let settled = data.get("isSettled")?.as_bool()?;
    let flag = |key: &str| data.get(key) == Some(&Value::Bool(true));
    let extra_usage = object(data.get("usageLimit"))
        .is_some_and(|limit| limit.get("extraUsage") == Some(&Value::Bool(true)));
    Some(ReadState {
        session_id: session_id.to_owned(),
        session_file: session_file.to_owned(),
        goal,
        settled,
        foreground: flag("isStreaming") || flag("isCompacting"),
        streaming: flag("isStreaming"),
        queued: data
            .get("queuedMessageCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        model: model_of(data.get("model")),
        thinking: string(data.get("thinkingLevel")).map(str::to_owned),
        extra_usage,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Question {
    id: String,
    #[serde(rename = "question")]
    text: String,
    options: Vec<String>,
    multi: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DialogKind {
    Approval,
    Confirm,
    Select,
    Input,
    Editor,
    Ask,
    /// An HTML form (the `ask` tool); answered with `data` or cancelled.
    Form,
    Unsupported,
}

/// A pending dialog; no kind carries both `options` and `secret`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Dialog {
    pub(crate) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    pub(crate) kind: DialogKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) options: Option<Vec<String>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) secret: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) questions: Option<Vec<Question>>,
    /// The form's HTML, for the relay that renders it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) html: Option<String>,
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
            html: None,
        }
    }

    /// Every visible word of the dialog, lower-cased, for the spend-offer test.
    fn text(&self) -> String {
        let mut all = String::new();
        for part in [&self.title, &self.message, &self.html]
            .into_iter()
            .flatten()
        {
            all.push_str(part);
            all.push('\n');
        }
        for option in self.options.iter().flatten() {
            all.push_str(option);
            all.push('\n');
        }
        for q in self.questions.iter().flatten() {
            all.push_str(&q.text);
            all.push('\n');
            for o in &q.options {
                all.push_str(o);
                all.push('\n');
            }
        }
        all.to_lowercase()
    }
}

/// Phrases that mark an offer to spend: redeem a saved reset, buy credits, enable overage (DESIGN 9.2).
const SPEND_PHRASES: [&str; 12] = [
    "redeem",
    "saved reset",
    "usage reset",
    "reset now",
    "buy credits",
    "purchase credits",
    "add credits",
    "extra usage",
    "overage",
    "pay as you go",
    "top up",
    "upgrade your plan",
];

/// Whether a dialog offers to spend money or a saved reset.
pub(crate) fn spend_offer(dialog: &Dialog, owner_phrases: &[String]) -> bool {
    let text = dialog.text();
    SPEND_PHRASES.iter().any(|p| text.contains(p))
        || owner_phrases
            .iter()
            .any(|p| !p.is_empty() && text.contains(p.as_str()))
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

/// What one `extension_ui_request` frame means for the pending dialogs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UiFrame {
    Open(Dialog),
    /// OMP closed the dialog with this ID.
    Cancel(String),
    /// A notice for the output ring.
    Notice(String),
    /// Presentation only.
    Ignore,
}

/// Classifies an `extension_ui_request` frame (HOST req. 42). Unknown methods (including `open_url`) are
/// `unsupported`: they block the session, but their content is not exposed and they cannot be answered.
pub(crate) fn ui_frame(frame: &Value) -> UiFrame {
    match string(frame.get("method")) {
        Some("cancel") => {
            return string(frame.get("targetId"))
                .or_else(|| string(frame.get("id")))
                .map_or(UiFrame::Ignore, |id| UiFrame::Cancel(id.to_owned()));
        }
        Some("notify") => {
            return string(frame.get("message"))
                .map_or(UiFrame::Ignore, |m| UiFrame::Notice(m.to_owned()));
        }
        Some("setStatus" | "setWidget" | "setTitle" | "set_editor_text") => return UiFrame::Ignore,
        _ => {}
    }
    dialog_of(frame).map_or(UiFrame::Ignore, UiFrame::Open)
}

/// Builds the dialog of an `extension_ui_request` frame; `None` for non-dialog methods.
pub(crate) fn dialog_of(frame: &Value) -> Option<Dialog> {
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
        Some("form") => Some(match string(frame.get("html")) {
            Some(html) => Dialog {
                html: Some(html.to_owned()),
                ..summary(DialogKind::Form)
            },
            None => Dialog::bare(id, DialogKind::Unsupported),
        }),
        Some("cancel" | "notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text") => {
            None
        }
        _ => Some(Dialog::bare(id, DialogKind::Unsupported)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EntryKind {
    Assistant,
    Output,
    Notice,
}

/// One output-ring entry (HOST req. 40).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Entry {
    pub(crate) kind: EntryKind,
    pub(crate) text: String,
}

const ENTRY_UNITS: usize = 2_000;

/// The last `limit` UTF-16 code units of `text`, never splitting a character.
pub(crate) fn last_units(text: &str, limit: usize) -> &str {
    let mut units = 0;
    for (at, c) in text.char_indices().rev() {
        units += c.len_utf16();
        if units > limit {
            return text.get(at + c.len_utf8()..).unwrap_or_default();
        }
    }
    text
}

/// Control characters other than newline and tab removed.
pub(crate) fn clean(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect()
}

/// The text parts of a message's content.
pub(crate) fn message_text(message: &Value) -> Option<String> {
    match message.get("content") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(content)) => Some(
            content
                .iter()
                .filter(|part| string(part.get("type")) == Some("text"))
                .filter_map(|part| string(part.get("text")))
                .collect(),
        ),
        _ => None,
    }
}

/// The output ring's view of an assistant message, command output or notice.
pub(crate) fn entry_of(frame: &Value) -> Option<Entry> {
    let message = frame.get("message");
    let kind_of = string(frame.get("type"));
    let (kind, text) = if kind_of == Some("message_end")
        && message.is_some_and(|m| string(m.get("role")) == Some("assistant"))
        && message.is_some_and(|m| m.get("content").is_some_and(Value::is_array))
    {
        (EntryKind::Assistant, message.and_then(message_text))
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
    Some(Entry {
        kind,
        text: last_units(&clean(&text), ENTRY_UNITS).to_owned(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AnswerItem {
    id: String,
    selected_options: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_input: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AnswerResponse {
    Cancelled(bool),
    Confirmed(bool),
    Value(String),
    Answers(Vec<AnswerItem>),
    Data(Map<String, Value>),
}

/// An `extension_ui_response` frame: `{type,id,<one response field>}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AnswerFrame {
    #[serde(rename = "type")]
    kind: &'static str,
    id: String,
    #[serde(flatten)]
    response: AnswerResponse,
}

impl AnswerFrame {
    /// The cancel answer, which every supported dialog accepts.
    pub(crate) fn cancel(id: &str) -> Self {
        Self {
            kind: "extension_ui_response",
            id: id.to_owned(),
            response: AnswerResponse::Cancelled(true),
        }
    }

    pub(crate) fn to_map(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        }
    }
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
    for (at, option) in selected.iter().enumerate() {
        if selected
            .get(..at)
            .is_some_and(|before| before.contains(option))
        {
            return None;
        }
    }
    let custom = match answer.get("customInput") {
        None => None,
        Some(Value::String(custom)) => Some(custom.clone()),
        Some(_) => return None,
    };
    let filled = custom
        .as_deref()
        .is_some_and(|custom| !custom.trim().is_empty());
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

fn form_value(value: &Value) -> bool {
    match value {
        Value::String(_) | Value::Number(_) => true,
        Value::Array(items) => items.iter().all(Value::is_string),
        _ => false,
    }
}

/// Exactly one recognized answer field for the dialog's kind (HOST req. 45-47). Unrecognized fields
/// (notably `timedOut`, which may submit recommended answers) are never forwarded, secrets are never
/// answered, and approvals grant one call only: no session or global grants.
pub(crate) fn answer_frame(dialog: &Dialog, answer: &Value) -> Option<AnswerFrame> {
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
    if dialog.kind == DialogKind::Form {
        let data = response.get("data")?.as_object()?;
        return data
            .values()
            .all(form_value)
            .then(|| AnswerResponse::Data(data.clone()))
            .and_then(frame);
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

/// A session's status (HOST req. 36).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Idle,
    Working,
    Blocked,
    Error,
    Unknown,
}

/// What the status is computed from.
// Independent facts the status is computed from.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Facts<'a> {
    /// The identity is bound.
    pub(crate) bound: bool,
    /// The last readback, when it is fresh.
    pub(crate) state: Option<&'a ReadState>,
    pub(crate) fault: bool,
    /// Held: model mismatch or another reason the owner must clear.
    pub(crate) held: bool,
    pub(crate) stopping: bool,
    pub(crate) dialogs: bool,
}

/// Status precedence is error, blocked, unknown, idle, working (HOST req. 36); input is blocked on a
/// dialog, fault, stopping, hold, no binding or no fresh readback (HOST req. 38).
pub(crate) fn status(f: &Facts<'_>) -> (Status, bool) {
    let blocked_input =
        f.dialogs || f.fault || f.stopping || f.held || !f.bound || f.state.is_none();
    let status = if f.fault {
        Status::Error
    } else if f.dialogs
        || f.state
            .is_some_and(|s| s.goal.status == GoalStatus::Blocked)
    {
        Status::Blocked
    } else if !f.bound || f.stopping {
        Status::Unknown
    } else {
        match f.state {
            None => Status::Unknown,
            Some(state) if state.idle() => Status::Idle,
            Some(_) => Status::Working,
        }
    };
    (status, blocked_input)
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
        assert_eq!(
            ui_frame(&json!({ "id": "x", "method": "cancel", "targetId": "d1" })),
            UiFrame::Cancel("d1".into())
        );
    }

    #[test]
    fn forms_take_only_plain_data() {
        let form = dialog(&json!({ "id": "f", "method": "form", "html": "<form></form>" }));
        assert_eq!(form.kind, DialogKind::Form);
        assert!(sent(&form, &json!({ "data": { "a": "x", "b": ["y"], "n": 2 } })).is_some());
        assert_eq!(sent(&form, &json!({ "data": { "a": { "x": 1 } } })), None);
        assert_eq!(sent(&form, &json!({ "data": "x" })), None);
        assert!(sent(&form, &json!({ "cancelled": true })).is_some());
        assert_eq!(
            dialog(&json!({ "id": "f", "method": "form" })).kind,
            DialogKind::Unsupported
        );
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
        assert_eq!(sent(&question, &json!({ "answers": [] })), None);
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
            entry_of(&json!({ "type": "command_output", "text": " \n" })),
            None
        );
        assert_eq!(entry_of(&json!({ "type": "agent_start" })), None);
        assert_eq!(last_units("abcdef", 3), "def");
    }

    #[test]
    fn goals_and_models_read_like_omp_reports_them() {
        assert_eq!(
            goal_of(Some(&Value::Null)).map(|g| g.status),
            Some(GoalStatus::None)
        );
        assert_eq!(goal_of(None), None);
        assert_eq!(goal_of(Some(&json!({}))), None);
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
        assert_eq!(
            model_of(Some(&json!({ "provider": "fixture", "id": "initial" }))).as_deref(),
            Some("fixture/initial")
        );
        assert_eq!(
            model_of(Some(&json!({ "provider": "a/b", "id": "c" }))),
            None
        );
        assert!(same_model("p/Model-A", "p/model-a"));
        assert!(!same_model("P/model-a", "p/model-a"));
    }

    fn fixture_state(settled: bool) -> Value {
        json!({
            "sessionId": "fixture-session", "sessionFile": "/fixture/session.jsonl", "isSettled": settled,
            "isStreaming": !settled, "goal": null, "queuedMessageCount": 0,
            "model": { "provider": "fixture", "id": "initial" }, "thinkingLevel": "low",
        })
    }

    #[test]
    fn read_state_requires_identity_goal_and_settlement() {
        let state = read_state(Some(&fixture_state(false))).unwrap();
        assert_eq!(state.session_id, "fixture-session");
        assert!(state.foreground && !state.settled && !state.idle());
        assert_eq!(state.model.as_deref(), Some("fixture/initial"));
        assert_eq!(state.thinking.as_deref(), Some("low"));
        let mut data = fixture_state(true);
        data["usageLimit"] = json!({ "stage": "wrap_up", "extraUsage": true });
        assert!(read_state(Some(&data)).unwrap().extra_usage);
        for key in ["sessionId", "sessionFile", "goal", "isSettled"] {
            let mut data = fixture_state(true);
            data.as_object_mut().unwrap().remove(key);
            assert_eq!(read_state(Some(&data)), None, "{key}");
        }
    }

    #[test]
    fn status_precedence() {
        let idle = read_state(Some(&fixture_state(true))).unwrap();
        let busy = read_state(Some(&fixture_state(false))).unwrap();
        let mut budget = idle.clone();
        budget.goal.status = GoalStatus::Blocked;
        let base = Facts {
            bound: true,
            state: Some(&idle),
            fault: false,
            held: false,
            stopping: false,
            dialogs: false,
        };
        assert_eq!(status(&base), (Status::Idle, false));
        assert_eq!(
            status(&Facts {
                state: Some(&busy),
                ..base
            }),
            (Status::Working, false)
        );
        assert_eq!(
            status(&Facts {
                state: Some(&budget),
                ..base
            }),
            (Status::Blocked, false)
        );
        assert_eq!(
            status(&Facts {
                dialogs: true,
                fault: true,
                ..base
            }),
            (Status::Error, true)
        );
        assert_eq!(
            status(&Facts {
                dialogs: true,
                ..base
            }),
            (Status::Blocked, true)
        );
        assert_eq!(
            status(&Facts {
                stopping: true,
                ..base
            }),
            (Status::Unknown, true)
        );
        assert_eq!(
            status(&Facts {
                state: None,
                ..base
            }),
            (Status::Unknown, true)
        );
        assert_eq!(status(&Facts { held: true, ..base }), (Status::Idle, true));
    }

    #[test]
    fn spend_offers_are_recognised() {
        let offer = dialog(&json!({
            "id": "s", "method": "select", "title": "Usage limit reached",
            "options": ["Redeem a saved reset", "Wait"],
        }));
        assert!(spend_offer(&offer, &[]));
        let plain = dialog(&fixture_approval());
        assert!(!spend_offer(&plain, &[]));
        assert!(spend_offer(&plain, &["fake write".to_string()]));
    }
}
