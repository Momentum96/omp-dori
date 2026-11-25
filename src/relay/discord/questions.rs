//! Questions and approvals on Discord (DISC-OUT-15 to 18, 20; DESIGN 12.2 A17, A18, C11). An open
//! dialog of the lead is posted to the lead's home channel: a placeholder first, then edited into the
//! question, with one button per choice when buttons can arrive over the Gateway. The owner answers
//! by pressing a button or by replying; anyone else gets an ephemeral refusal. Whichever answer comes
//! first wins: a local answer closes the Discord question, and a press after that is refused. A
//! single recommended choice is submitted explicitly after the configured quiet time (3 minutes by
//! default). Tool approvals are posted only when the owner turned remote approvals on, as a reply to
//! the request that started the run, and deny on any doubt.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

use super::form::{Field, FieldKind, Form};
use super::intake::{Intake, NAME};
use super::types::Press;
use super::{Snowflake, nonce};
use crate::host::dialogs::DialogKind;
use crate::host::dialogs::Question;
use crate::host::{Dialog, Runtime};
use crate::log;
use crate::relay::Inbound;
use crate::relay::text::DISCORD_LIMIT;
use crate::store::relay::{QueueRow, RelayWrite};

/// The question summary may be at most this long (DISC-OUT req. 63).
const SUMMARY_MAX: usize = 1500;
/// Buttons at most: 5 rows of 5 (DISC-OUT req. 67).
const BUTTONS_MAX: usize = 25;
const LABEL_MAX: usize = 80;
/// Finishing edits are best effort within this budget (DISC-OUT req. 75).
const EDIT_BUDGET: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// The `ask` tool with one question.
    Ask {
        question: String,
        multi: bool,
    },
    /// An HTML form with one editable field: its name, the values behind the choices, the hidden
    /// fields returned unchanged, and whether it takes free text or several choices.
    Form {
        field: String,
        values: Vec<String>,
        hidden: serde_json::Map<String, Value>,
        free: bool,
        multi: bool,
    },
    /// An HTML form with several editable fields, answered by a reply holding a JSON map of field
    /// name to value (DISC-OUT req. 63, 70, 72); hidden fields are returned unchanged.
    Fields {
        fields: Vec<Field>,
        hidden: serde_json::Map<String, Value>,
    },
    /// The `ask` tool with several questions, answered by a reply holding a JSON map of question ID
    /// to answer, like a form with several fields.
    Asks {
        questions: Vec<Question>,
    },
    Select,
    Confirm,
    Approval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    /// An answer is being acknowledged or submitted; other presses are ignored.
    Taking,
}

#[derive(Debug, Clone)]
struct Card {
    dialog: String,
    channel: Snowflake,
    message: Snowflake,
    kind: Kind,
    choices: Vec<String>,
    body: String,
    stamp: String,
    state: State,
}

/// The open cards of this run.
#[derive(Default)]
pub(crate) struct Cards(Mutex<Inner>);

#[derive(Default)]
struct Inner {
    open: HashMap<String, Card>,
    /// Dialogs answered locally only (not convertible, no route, or a failed post).
    skipped: HashSet<String>,
}

impl Cards {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Moves a card from open to taking; `None` when it is gone or already being answered.
    fn take(&self, dialog: &str) -> Option<Card> {
        let mut inner = self.lock();
        let card = inner.open.get_mut(dialog)?;
        if card.state != State::Open {
            return None;
        }
        card.state = State::Taking;
        Some(card.clone())
    }

    fn reopen(&self, dialog: &str) {
        if let Some(card) = self.lock().open.get_mut(dialog) {
            card.state = State::Open;
        }
    }

    fn remove(&self, dialog: &str) -> Option<Card> {
        self.lock().open.remove(dialog)
    }

    fn by_message(&self, message: Snowflake) -> Option<Card> {
        self.lock()
            .open
            .values()
            .find(|c| c.message == message)
            .cloned()
    }
}

/// Keeps text from mentioning anyone, opening code spans or forming Markdown (DISC-OUT req. 63).
fn neutral(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' | '*' | '_' | '~' | '`' | '|' | '>' | '#' | '[' | ']' | '(' | ')' => {
                out.push('\\');
                out.push(c);
            }
            '@' => out.push_str("@\u{200b}"),
            _ => out.push(c),
        }
    }
    out
}

/// The card for a dialog, or `None` when it stays local (DISC-OUT req. 59-64).
fn card_of(
    dialog: &Dialog,
    remote_approvals: bool,
) -> Option<(Kind, String, Vec<String>, Option<usize>)> {
    let lines = |head: &str| {
        let mut text = format!("**{head}**");
        for part in [&dialog.title, &dialog.message].into_iter().flatten() {
            text.push('\n');
            text.push_str(&neutral(part));
        }
        text
    };
    match dialog.kind {
        DialogKind::Ask => {
            let [q] = dialog.questions.as_deref()? else {
                return asks_card(dialog.questions.as_deref()?);
            };
            if q.options.is_empty() || q.options.len() > BUTTONS_MAX {
                return None;
            }
            let text = format!("**Question from the lead**\n{}", neutral(&q.text));
            Some((
                Kind::Ask {
                    question: q.id.clone(),
                    multi: q.multi,
                },
                text,
                q.options.clone(),
                q.recommended.filter(|_| !q.multi),
            ))
        }
        DialogKind::Form => {
            let form = super::form::read(dialog.html.as_deref()?).ok()?;
            let hidden: serde_json::Map<String, Value> = form
                .fields
                .iter()
                .filter(|f| f.kind == FieldKind::Hidden)
                .map(|f| (f.name.clone(), Value::String(f.value.clone())))
                .collect();
            let editable = form.editable();
            let [field] = editable[..] else {
                return fields_card(&form, hidden);
            };
            let selected: Vec<usize> = field
                .choices
                .iter()
                .enumerate()
                .filter(|(_, c)| c.selected)
                .map(|(i, _)| i)
                .collect();
            let multi = field.kind == FieldKind::Many;
            let mut text = format!("**Question from the lead**\n{}", neutral(&form.text));
            if !field.label.is_empty() && !form.text.contains(&field.label) {
                text.push('\n');
                text.push_str(&neutral(&field.label));
            }
            Some((
                Kind::Form {
                    field: field.name.clone(),
                    values: field.choices.iter().map(|c| c.value.clone()).collect(),
                    hidden,
                    free: field.kind == FieldKind::Text,
                    multi,
                },
                text,
                field.choices.iter().map(|c| c.label.clone()).collect(),
                match selected.as_slice() {
                    [one] if !multi => Some(*one),
                    _ => None,
                },
            ))
        }
        DialogKind::Select => {
            let options = dialog.options.clone()?;
            (!options.is_empty() && options.len() <= BUTTONS_MAX).then(|| {
                (
                    Kind::Select,
                    lines("Choice asked by the lead"),
                    options,
                    None,
                )
            })
        }
        DialogKind::Confirm => Some((
            Kind::Confirm,
            lines("Confirmation asked by the lead"),
            vec!["Yes".into(), "No".into()],
            None,
        )),
        DialogKind::Approval if remote_approvals && !dialog.secret => Some((
            Kind::Approval,
            lines("Permission asked"),
            vec!["Approve once".into(), "Deny".into()],
            None,
        )),
        _ => None,
    }
}

/// A form with several editable fields (DISC-OUT req. 63): its text, then per field the name, label,
/// kind, whether it is required, the choices with their exact values and the default.
fn fields_card(
    form: &Form,
    hidden: serde_json::Map<String, Value>,
) -> Option<(Kind, String, Vec<String>, Option<usize>)> {
    let fields: Vec<Field> = form.editable().into_iter().cloned().collect();
    if fields.is_empty() {
        return None;
    }
    let mut text = format!("**Question from the lead**\n{}", neutral(&form.text));
    for f in &fields {
        let kind = match f.kind {
            FieldKind::One => "one choice",
            FieldKind::Many => "a list of choices",
            FieldKind::Text | FieldKind::Hidden => "text",
        };
        let need = if f.required { "required" } else { "optional" };
        let _ = write!(text, "\n- {}", neutral(&f.name));
        if !f.label.is_empty() && f.label != f.name {
            let _ = write!(text, " ({})", neutral(&f.label));
        }
        let _ = write!(text, ": {kind}, {need}");
        if !f.choices.is_empty() {
            let list: Vec<String> = f
                .choices
                .iter()
                .map(|c| format!("{} = {}", neutral(&c.value), neutral(&c.label)))
                .collect();
            let _ = write!(text, "; choices: {}", list.join(", "));
        }
        match default_of(f) {
            Value::String(s) if !s.is_empty() => {
                let _ = write!(text, "; default: {}", neutral(&s));
            }
            Value::Array(a) if !a.is_empty() => {
                let values: Vec<String> = a.iter().filter_map(Value::as_str).map(neutral).collect();
                let _ = write!(text, "; default: {}", values.join(", "));
            }
            _ => {}
        }
    }
    Some((Kind::Fields { fields, hidden }, text, Vec::new(), None))
}

/// A field's default as OMP takes it: the selected value of a choice, the selected values of a list,
/// or the initial text (DISC-OUT req. 62).
fn default_of(f: &Field) -> Value {
    let mut selected = f
        .choices
        .iter()
        .filter(|c| c.selected)
        .map(|c| Value::String(c.value.clone()));
    match f.kind {
        FieldKind::One => selected.next().unwrap_or(Value::Null),
        FieldKind::Many => Value::Array(selected.collect()),
        FieldKind::Text | FieldKind::Hidden => Value::String(f.value.clone()),
    }
}

/// One answered field, checked against the field (DISC-OUT req. 72): a choice by its exact value or,
/// uniquely, its visible label; a list as an array of choices; text as a string or a number.
fn field_value(f: &Field, value: Value) -> Result<Value, String> {
    let choice = |item: &Value| -> Option<Value> {
        let s = match item {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return None,
        };
        if let Some(c) = f.choices.iter().find(|c| c.value == s) {
            return Some(Value::String(c.value.clone()));
        }
        let hits: Vec<_> = f
            .choices
            .iter()
            .filter(|c| c.label.eq_ignore_ascii_case(&s))
            .collect();
        match hits.as_slice() {
            [one] => Some(Value::String(one.value.clone())),
            _ => None,
        }
    };
    let name = neutral(&f.name);
    match f.kind {
        FieldKind::Text | FieldKind::Hidden => match value {
            Value::String(s) => Ok(Value::String(s)),
            Value::Number(n) => Ok(Value::String(n.to_string())),
            _ => Err(format!("{name} takes text")),
        },
        FieldKind::One => choice(&value).ok_or_else(|| format!("{name} takes one of its choices")),
        FieldKind::Many => {
            let items = match value {
                Value::Array(items) => items,
                one => vec![one],
            };
            items
                .iter()
                .map(choice)
                .collect::<Option<Vec<Value>>>()
                .map(Value::Array)
                .ok_or_else(|| format!("{name} takes a list of its choices"))
        }
    }
}

/// The answer OMP takes for choice `index`.
fn answer_of(card: &Card, dialog: &Dialog, index: usize) -> Option<Value> {
    let label = card.choices.get(index)?;
    Some(match &card.kind {
        Kind::Ask { question, .. } => {
            json!({"answers": [{"id": question, "selectedOptions": [label]}]})
        }
        Kind::Fields { .. } | Kind::Asks { .. } => return None,
        Kind::Form {
            field,
            values,
            hidden,
            multi,
            ..
        } => {
            let value = values.get(index)?;
            form_data(
                field,
                hidden,
                if *multi { json!([value]) } else { json!(value) },
            )
        }
        Kind::Select => json!({ "value": label }),
        Kind::Confirm => json!({ "confirmed": index == 0 }),
        Kind::Approval => {
            let has = |o: &str| dialog.options.iter().flatten().any(|x| x == o);
            let approve = if has("Approve Once") {
                "Approve Once"
            } else {
                "Approve"
            };
            json!({ "value": if index == 0 { approve } else { "Deny" } })
        }
    })
}

/// A JSON map answering a form with several fields (DISC-OUT req. 70, 72): only editable fields,
/// omitted ones keep their defaults, required ones end up non-empty.
fn fields_answer(
    fields: &[Field],
    hidden: &serde_json::Map<String, Value>,
    text: &str,
) -> Result<Value, String> {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) else {
        return Err(
            "reply with a JSON map of field name to value, such as {\"field\": \"value\"}".into(),
        );
    };
    let mut data = hidden.clone();
    for f in fields {
        data.insert(f.name.clone(), default_of(f));
    }
    for (name, value) in map {
        let Some(f) = fields.iter().find(|f| f.name == name) else {
            return Err(format!("{} is not a field of this form", neutral(&name)));
        };
        data.insert(name, field_value(f, value)?);
    }
    for f in fields.iter().filter(|f| f.required) {
        let empty = match data.get(&f.name) {
            None | Some(Value::Null) => true,
            Some(Value::String(s)) => s.is_empty(),
            Some(Value::Array(a)) => a.is_empty(),
            Some(_) => false,
        };
        if empty {
            return Err(format!("{} is required", neutral(&f.name)));
        }
    }
    Ok(json!({ "data": data }))
}

/// An `ask` dialog with several questions: each with its ID, choices and recommended choice.
fn asks_card(questions: &[Question]) -> Option<(Kind, String, Vec<String>, Option<usize>)> {
    if questions.is_empty() {
        return None;
    }
    let mut text = String::from("**Questions from the lead**");
    for q in questions {
        let _ = write!(text, "\n- {}: {}", neutral(&q.id), neutral(&q.text));
        if !q.options.is_empty() {
            let list: Vec<String> = q.options.iter().map(|o| neutral(o)).collect();
            let many = if q.multi { "any of" } else { "one of" };
            let _ = write!(text, " ({many}: {})", list.join(", "));
        }
        if let Some(r) = q.recommended.and_then(|r| q.options.get(r)) {
            let _ = write!(text, "; default: {}", neutral(r));
        }
    }
    Some((
        Kind::Asks {
            questions: questions.to_vec(),
        },
        text,
        Vec::new(),
        None,
    ))
}

/// A JSON map answering an `ask` dialog with several questions: a choice by its label (a list for
/// several), or free text; an omitted question takes its recommended choice or is refused.
fn asks_answer(questions: &[Question], text: &str) -> Result<Value, String> {
    let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(text) else {
        return Err(
            "reply with a JSON map of question to answer, such as {\"q1\": \"yes\"}".into(),
        );
    };
    if let Some(name) = map.keys().find(|k| !questions.iter().any(|q| &q.id == *k)) {
        return Err(format!("{} is not a question here", neutral(name)));
    }
    let mut answers = Vec::with_capacity(questions.len());
    for q in questions {
        let given = map.remove(&q.id);
        let items: Vec<String> = match given {
            None => match q.recommended.and_then(|r| q.options.get(r)) {
                Some(r) => vec![r.clone()],
                None => return Err(format!("{} is not answered", neutral(&q.id))),
            },
            Some(Value::Array(items)) if q.multi => items
                .iter()
                .map(|i| i.as_str().map(str::to_string))
                .collect::<Option<Vec<String>>>()
                .ok_or_else(|| format!("{} takes a list of texts", neutral(&q.id)))?,
            Some(Value::String(s)) => vec![s],
            Some(_) => return Err(format!("{} takes text", neutral(&q.id))),
        };
        let picked: Option<Vec<&String>> = items
            .iter()
            .map(|i| q.options.iter().find(|o| o.eq_ignore_ascii_case(i.trim())))
            .collect();
        answers.push(match picked {
            Some(labels) => json!({"id": q.id, "selectedOptions": labels}),
            None => json!({"id": q.id, "selectedOptions": [], "customInput": items.join(", ")}),
        });
    }
    Ok(json!({ "answers": answers }))
}

fn form_data(field: &str, hidden: &serde_json::Map<String, Value>, value: Value) -> Value {
    let mut data = hidden.clone();
    data.insert(field.to_string(), value);
    json!({ "data": data })
}

/// A typed answer (DISC-OUT req. 70, 72): an option number, a unique label, for `ask` free text, or
/// for a form with several fields a JSON map of field name to value.
fn typed_answer(card: &Card, text: &str) -> Result<Value, String> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > 2000 {
        return Err("answers are 1 to 2000 characters".into());
    }
    let pick = |item: &str| -> Option<usize> {
        let item = item.trim();
        if let Ok(n) = item.parse::<usize>() {
            return (1..=card.choices.len()).contains(&n).then(|| n - 1);
        }
        let hits: Vec<usize> = card
            .choices
            .iter()
            .enumerate()
            .filter(|(_, c)| c.eq_ignore_ascii_case(item))
            .map(|(i, _)| i)
            .collect();
        match hits.as_slice() {
            [one] => Some(*one),
            _ => None,
        }
    };
    match &card.kind {
        Kind::Ask { question, multi } => {
            let items: Vec<&str> = if *multi {
                text.split(',').collect()
            } else {
                vec![text]
            };
            let picked: Option<Vec<&String>> = items
                .iter()
                .map(|i| pick(i).and_then(|n| card.choices.get(n)))
                .collect();
            Ok(match picked {
                Some(labels) => json!({"answers": [{"id": question, "selectedOptions": labels}]}),
                None => {
                    json!({"answers": [{"id": question, "selectedOptions": [], "customInput": text}]})
                }
            })
        }
        Kind::Fields { fields, hidden } => fields_answer(fields, hidden, text),
        Kind::Asks { questions } => asks_answer(questions, text),
        Kind::Form {
            field,
            values,
            hidden,
            free,
            multi,
        } => {
            if *free {
                return Ok(form_data(field, hidden, json!(text)));
            }
            let items: Vec<&str> = if *multi {
                text.split(',').collect()
            } else {
                vec![text]
            };
            let picked: Option<Vec<&String>> = items
                .iter()
                .map(|i| pick(i).and_then(|n| values.get(n)))
                .collect();
            match picked {
                Some(v) if *multi => Ok(form_data(field, hidden, json!(v))),
                Some(v) => Ok(form_data(field, hidden, json!(v.first()))),
                None => Err(format!(
                    "answer with a number from 1 to {}",
                    card.choices.len()
                )),
            }
        }
        Kind::Select | Kind::Confirm => match pick(text) {
            Some(i) if card.kind == Kind::Select => Ok(json!({ "value": card.choices.get(i) })),
            Some(i) => Ok(json!({ "confirmed": i == 0 })),
            None => Err(format!(
                "answer with a number from 1 to {}",
                card.choices.len()
            )),
        },
        Kind::Approval => Err("approvals are answered with the buttons".into()),
    }
}

fn buttons(stamp: &str, choices: &[String], recommended: Option<usize>, approval: bool) -> Value {
    let row_of = |chunk: &[(usize, &String)]| {
        let components: Vec<Value> = chunk
            .iter()
            .map(|(i, label)| {
                let label = if label.chars().count() > LABEL_MAX {
                    format!("Option {}", i + 1)
                } else {
                    (*label).clone()
                };
                let style = match (approval, *i) {
                    (true, 0) => 3,
                    (true, _) => 4,
                    (false, i) if Some(i) == recommended => 1,
                    _ => 2,
                };
                json!({"type": 2, "style": style, "label": label, "custom_id": format!("cy:{stamp}:{i}")})
            })
            .collect();
        json!({"type": 1, "components": components})
    };
    let indexed: Vec<(usize, &String)> = choices.iter().enumerate().collect();
    Value::Array(indexed.chunks(5).map(row_of).collect())
}

fn choice_lines(choices: &[String], recommended: Option<usize>) -> String {
    choices
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let tag = if Some(i) == recommended {
                " (recommended)"
            } else {
                ""
            };
            format!("{}. {}{tag}", i + 1, neutral(c))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn duration_words(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 60 && s.is_multiple_of(60) {
        format!("{} minute(s)", s / 60)
    } else {
        format!("{s} s")
    }
}

impl<T: Runtime> Intake<T> {
    async fn edit_closing(&self, channel: Snowflake, message: Snowflake, content: &str) {
        let body = json!({ "content": content, "components": [] });
        match tokio::time::timeout(EDIT_BUDGET, self.rest.edit(channel, message, body)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::info("question not updated", &[("error", &e)]),
            Err(_) => log::info("question not updated within 3 s", &[]),
        }
        let write = RelayWrite::Question {
            relay: NAME.into(),
            room: channel.to_string(),
            message: message.to_string(),
            open: false,
        };
        if let Err(e) = self.store.relay(write).await {
            log::warn("question record not closed", &[("error", &e)]);
        }
    }

    /// Questions left open by an earlier run are closed (DISC-OUT req. 76).
    pub(super) async fn close_left_open(&self) {
        let left = self
            .store
            .read(|r| r.open_questions(NAME))
            .await
            .unwrap_or_default();
        for (room, message) in left {
            if let (Some(c), Some(m)) = (Snowflake::parse(&room), Snowflake::parse(&message)) {
                self.edit_closing(
                    c,
                    m,
                    "Closed: Cataly restarted before this was answered here.",
                )
                .await;
            }
        }
    }

    /// Keeps the cards in step with the lead's dialogs: new ones are posted, gone ones closed.
    pub(super) async fn sync_cards(self: &Arc<Self>) {
        let dialogs: Vec<Dialog> = self
            .lead
            .snapshot()
            .map_or_else(Vec::new, |s| s.dialogs.clone());
        let live: HashSet<&str> = dialogs.iter().map(|d| d.id.as_str()).collect();
        let gone: Vec<Card> = {
            let inner = self.cards.lock();
            inner
                .open
                .values()
                .filter(|c| c.state == State::Open && !live.contains(c.dialog.as_str()))
                .cloned()
                .collect()
        };
        for card in gone {
            if self.cards.take(&card.dialog).is_none() {
                continue;
            }
            self.cards.remove(&card.dialog);
            let status = if card.kind == Kind::Approval {
                "Denied: the run ended before an answer."
            } else {
                "Closed: answered elsewhere."
            };
            self.edit_closing(
                card.channel,
                card.message,
                &format!("{}\n\n{status}", card.body),
            )
            .await;
        }
        for dialog in dialogs {
            let known = {
                let inner = self.cards.lock();
                inner.open.contains_key(&dialog.id) || inner.skipped.contains(&dialog.id)
            };
            if !known {
                self.post_card(&dialog).await;
            }
        }
    }

    /// An approval answers the request that started the run; a question goes to the home channel.
    async fn route(&self, approval: bool) -> Option<(Snowflake, Option<Snowflake>)> {
        if !approval {
            return self.settings.home().map(|c| (c, None));
        }
        let open = self
            .store
            .read(|r| r.answers_open(NAME))
            .await
            .unwrap_or_default();
        open.into_iter()
            .rev()
            .find(|h| h.answer == "waiting")
            .and_then(|h| Some((Snowflake::parse(&h.room)?, Snowflake::parse(&h.message))))
    }

    /// The question text with its choices and, for a recommended choice, when it is used.
    fn body(
        &self,
        head: &str,
        choices: &[String],
        recommended: Option<usize>,
        kind: &Kind,
    ) -> String {
        let mut body = if choices.is_empty() {
            head.to_string()
        } else {
            format!("{head}\n{}", choice_lines(choices, recommended))
        };
        match kind {
            Kind::Approval => {}
            Kind::Fields { .. } | Kind::Asks { .. } => body.push_str(
                "\nReply to this message with a JSON map of name to value; omitted ones keep their defaults.",
            ),
            _ => body.push_str("\nReply to this message with a number or your answer."),
        }
        if let Some(r) = recommended {
            let _ = write!(
                body,
                "\nWithout an answer in {}, choice {} (recommended) is used.",
                duration_words(self.settings.form_default_after),
                r + 1
            );
        }
        body
    }

    /// The first message of a card: an approval card as a reply, or a question's placeholder.
    async fn first_message(
        &self,
        dialog: &Dialog,
        channel: Snowflake,
        reply_to: Option<Snowflake>,
        body: &str,
        stamp: &str,
        components: Option<&Value>,
    ) -> Result<Value, super::RestError> {
        let approval = dialog.kind == DialogKind::Approval;
        if approval {
            let mut create = json!({
                "content": body,
                "nonce": nonce(&format!("approval:{}:{}:{stamp}", self.bot, dialog.id), 0),
                "components": components.cloned().unwrap_or(Value::Array(Vec::new())),
            });
            if let Some(source) = reply_to {
                super::put(
                    &mut create,
                    "message_reference",
                    json!({"message_id": source.to_string(),
                "channel_id": channel.to_string(), "fail_if_not_exists": true}),
                );
            }
            self.rest.create(channel, create).await
        } else {
            // A placeholder first, so the question's ID is known before any answer (DISC-OUT req. 66).
            let placeholder = json!({
                "content": "A question from the lead follows.",
                // Dialog IDs may repeat across sessions; the random stamp keeps each question its own.
                "nonce": nonce(&format!("question:{}:{}:{stamp}", self.bot, dialog.id), 0),
            });
            self.rest.create(channel, placeholder).await
        }
    }

    /// Button IDs: random per question; for approvals derived from bot, message and request IDs
    /// (DISC-OUT req. 81).
    fn stamp(&self, approval: bool, reply_to: Option<Snowflake>, dialog: &str) -> String {
        if approval {
            let seed = format!(
                "{}:{}:{dialog}",
                self.bot,
                reply_to.map_or(0, Snowflake::seq)
            );
            nonce(&seed, 0).chars().take(16).collect()
        } else {
            crate::host::random_hex(6)
        }
    }

    /// Edits a question's placeholder into the question; `false` when that failed and it stays local.
    async fn show(
        &self,
        dialog: &str,
        channel: Snowflake,
        message: Snowflake,
        body: &str,
        components: Option<Value>,
    ) -> bool {
        let edit = json!({
            "content": body,
            "components": components.unwrap_or(Value::Array(Vec::new())),
        });
        let Err(e) = self.rest.edit(channel, message, edit).await else {
            return true;
        };
        log::warn("question not shown; it stays local", &[("error", &e)]);
        self.cards.remove(dialog);
        self.cards.lock().skipped.insert(dialog.to_string());
        self.edit_closing(
            channel,
            message,
            "Closed: this question could not be shown here.",
        )
        .await;
        false
    }

    /// Posts one dialog, or marks it local only.
    async fn post_card(self: &Arc<Self>, dialog: &Dialog) {
        let skip = |me: &Self| {
            me.cards.lock().skipped.insert(dialog.id.clone());
        };
        let Some((kind, head, choices, recommended)) =
            card_of(dialog, self.settings.remote_approvals)
        else {
            return skip(self);
        };
        let approval = kind == Kind::Approval;
        let route = self.route(approval).await;
        let Some((channel, reply_to)) = route else {
            if approval {
                self.deny(dialog, "no Discord request started this run")
                    .await;
            }
            return skip(self);
        };
        let body = self.body(&head, &choices, recommended, &kind);
        if body.chars().count() > SUMMARY_MAX {
            if approval {
                self.deny(dialog, "the request is too long to show").await;
            }
            return skip(self);
        }
        let multi = matches!(
            kind,
            Kind::Ask { multi: true, .. }
                | Kind::Form { multi: true, .. }
                | Kind::Form { free: true, .. }
                | Kind::Fields { .. }
                | Kind::Asks { .. }
        );
        let stamp = self.stamp(approval, reply_to, &dialog.id);
        let components = (self.buttons && !multi && body.encode_utf16().count() <= DISCORD_LIMIT)
            .then(|| buttons(&stamp, &choices, recommended, approval));
        // The card exists before its buttons can be seen, so no press can find it missing.
        let first = self
            .first_message(
                dialog,
                channel,
                reply_to,
                &body,
                &stamp,
                components.as_ref(),
            )
            .await;
        let message = match first {
            Ok(v) => v
                .get("id")
                .and_then(Value::as_str)
                .and_then(Snowflake::parse),
            Err(e) => {
                log::warn("question not posted; it stays local", &[("error", &e)]);
                None
            }
        };
        let Some(message) = message else {
            if approval {
                // Fail closed (DISC-OUT req. 83).
                self.deny(dialog, "the approval card could not be posted")
                    .await;
            }
            return skip(self);
        };
        let card = Card {
            dialog: dialog.id.clone(),
            channel,
            message,
            kind,
            choices,
            body: body.clone(),
            stamp,
            state: State::Open,
        };
        self.cards.lock().open.insert(dialog.id.clone(), card);
        let write = RelayWrite::Question {
            relay: NAME.into(),
            room: channel.to_string(),
            message: message.to_string(),
            open: true,
        };
        if let Err(e) = self.store.relay(write).await {
            log::warn("question not recorded", &[("error", &e)]);
        }
        if !approval
            && !self
                .show(&dialog.id, channel, message, &body, components)
                .await
        {
            return;
        }
        if let Some(index) = recommended {
            let me = Arc::clone(self);
            let id = dialog.id.clone();
            tokio::spawn(async move { me.recommended_after(&id, index).await });
        }
    }

    async fn deny(&self, dialog: &Dialog, why: &str) {
        log::warn("remote approval denied", &[("reason", &why)]);
        if let Err(e) = self
            .lead
            .answer(&dialog.id, &json!({ "value": "Deny" }))
            .await
        {
            log::warn("approval not denied", &[("error", &e)]);
        }
    }

    /// The single recommended choice after the quiet time, submitted explicitly (DESIGN 12.2 C11).
    async fn recommended_after(&self, dialog: &str, index: usize) {
        tokio::time::sleep(self.settings.form_default_after).await;
        let Some(card) = self.cards.take(dialog) else {
            return;
        };
        let live = self
            .lead
            .snapshot()
            .and_then(|s| s.dialogs.iter().find(|d| d.id == dialog).cloned());
        let Some(live) = live else {
            self.cards.reopen(dialog);
            return;
        };
        let Some(answer) = answer_of(&card, &live, index) else {
            self.cards.reopen(dialog);
            return;
        };
        let label = card.choices.get(index).cloned().unwrap_or_default();
        let status = match self.lead.answer(dialog, &answer).await {
            Ok(()) => format!(
                "No answer within {}: the recommended choice \"{}\" was used.",
                duration_words(self.settings.form_default_after),
                neutral(&label)
            ),
            Err(e) => format!("Closed: {}", neutral(&e)),
        };
        self.cards.remove(dialog);
        self.edit_closing(
            card.channel,
            card.message,
            &format!("{}\n\n{status}", card.body),
        )
        .await;
    }

    async fn refuse(&self, press: &Press, text: &str) {
        let body = json!({"type": 4, "data": {"content": text, "flags": 64}});
        if let Err(e) = self.rest.callback(&press.id, &press.token, body).await {
            log::info("refusal not sent", &[("error", &e)]);
        }
    }

    /// A button press (DISC-OUT req. 69, 82): the owner's press on an open card answers it; anyone
    /// else's gets an ephemeral refusal; presses while one is being taken are ignored.
    pub(super) async fn press(self: Arc<Self>, press: Press) {
        let Some(card) = self.cards.by_message(press.message) else {
            return self.refuse(&press, "This question is closed.").await;
        };
        if !press.by_owner(self.application, self.owner) || press.channel != card.channel {
            return self.refuse(&press, "Only the owner can answer this.").await;
        }
        let index = press
            .custom_id
            .strip_prefix(&format!("cy:{}:", card.stamp))
            .and_then(|i| i.parse::<usize>().ok())
            .filter(|i| *i < card.choices.len());
        let Some(index) = index else {
            return self
                .refuse(&press, "This button is not part of the question.")
                .await;
        };
        let Some(card) = self.cards.take(&card.dialog) else {
            return;
        };
        let label = card.choices.get(index).cloned().unwrap_or_default();
        let done = format!(
            "{}\n\nAnswered by the owner: {}",
            card.body,
            neutral(&label)
        );
        let ack = if card.kind == Kind::Approval {
            json!({"type": 7, "data": {"content": done, "components": []}})
        } else {
            json!({"type": 6})
        };
        if let Err(e) = self.rest.callback(&press.id, &press.token, ack).await {
            // The answer counts only after a successful acknowledgement.
            log::info("press not acknowledged", &[("error", &e)]);
            return self.cards.reopen(&card.dialog);
        }
        self.submit(card, index, done).await;
    }

    /// Submits choice `index` of a taken card and closes the card.
    async fn submit(&self, card: Card, index: usize, done: String) {
        let live = self
            .lead
            .snapshot()
            .and_then(|s| s.dialogs.iter().find(|d| d.id == card.dialog).cloned());
        let outcome = match live.as_ref().and_then(|d| answer_of(&card, d, index)) {
            Some(answer) => self.lead.answer(&card.dialog, &answer).await,
            None => Err("the question was already answered".into()),
        };
        self.cards.remove(&card.dialog);
        let content = match outcome {
            Ok(()) => done,
            Err(e) => format!("{}\n\nClosed: {}", card.body, neutral(&e)),
        };
        self.edit_closing(card.channel, card.message, &content)
            .await;
    }

    /// An owner message that answers an open question (DISC-OUT req. 70-73): a reply to the question,
    /// or a plain message in its channel when exactly one question is open there.
    pub(super) async fn answer_by_text(&self, row: &QueueRow, inbound: &Inbound) -> bool {
        let target = {
            let inner = self.cards.lock();
            let here: Vec<&Card> = inner
                .open
                .values()
                .filter(|c| c.kind != Kind::Approval && c.channel.to_string() == row.room)
                .collect();
            let replied = here
                .iter()
                .find(|c| inbound.reply_to.as_deref() == Some(c.message.to_string().as_str()));
            match (replied, here.as_slice()) {
                (Some(c), _) => Some((*c).clone()),
                (None, [one])
                    if inbound.plain
                        && row.seq > one.message.seq()
                        && !matches!(one.kind, Kind::Fields { .. } | Kind::Asks { .. }) =>
                {
                    Some((*one).clone())
                }
                _ => None,
            }
        };
        let Some(card) = target else {
            return false;
        };
        let Some(card) = self.cards.take(&card.dialog) else {
            return false;
        };
        let answer = match typed_answer(&card, &inbound.text) {
            Ok(a) => a,
            Err(problem) => {
                self.cards.reopen(&card.dialog);
                let text = format!("That answer was not taken: {problem}. Please try again.");
                let seed = format!("correction:{}:{}", self.bot, row.message);
                if let crate::relay::PartOutcome::Rejected(e)
                | crate::relay::PartOutcome::Uncertain(e) = self
                    .rest
                    .reply_part(
                        card.channel,
                        Snowflake::parse(&row.message),
                        &text,
                        &nonce(&seed, 0),
                    )
                    .await
                {
                    log::info("correction not posted", &[("error", &e)]);
                }
                return true;
            }
        };
        let live = self
            .lead
            .snapshot()
            .and_then(|s| s.dialogs.iter().any(|d| d.id == card.dialog).then_some(()));
        let outcome = match live {
            Some(()) => self.lead.answer(&card.dialog, &answer).await,
            None => Err("the question was already answered".into()),
        };
        self.cards.remove(&card.dialog);
        let content = match outcome {
            Ok(()) => format!("{}\n\nAnswered by the owner's reply.", card.body),
            Err(e) => format!("{}\n\nClosed: {}", card.body, neutral(&e)),
        };
        self.edit_closing(card.channel, card.message, &content)
            .await;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::dialogs::dialog_of;

    fn ask(recommended: Option<u64>, multi: bool) -> Dialog {
        let mut q = json!({"id": "q1", "question": "Pick @everyone `x`", "multi": multi,
            "options": [{"label": "Red"}, {"label": "Blue"}]});
        if let Some(r) = recommended {
            q["recommended"] = json!(r);
        }
        dialog_of(
            &json!({"type": "extension_ui_request", "id": "d1", "method": "ask", "questions": [q]}),
        )
        .unwrap()
    }

    fn card(dialog: &Dialog) -> Card {
        let (kind, body, choices, _) = card_of(dialog, false).unwrap();
        Card {
            dialog: dialog.id.clone(),
            channel: Snowflake::parse("7").unwrap(),
            message: Snowflake::parse("9").unwrap(),
            kind,
            choices,
            body,
            stamp: "s".into(),
            state: State::Open,
        }
    }

    #[test]
    fn omp_forms_become_questions() {
        let html = r#"<form><fieldset><legend>Pick a color</legend><label><input type="radio" name="color" value="r"> Red</label><label><input type="radio" name="color" value="b" checked> Blue</label></fieldset><input type="hidden" name="k" value="v"></form>"#;
        let d = dialog_of(&json!({"id": "f", "method": "form", "html": html})).unwrap();
        let c = card(&d);
        assert_eq!(c.choices, vec!["Red", "Blue"]);
        assert_eq!(card_of(&d, false).unwrap().3, Some(1));
        assert_eq!(
            answer_of(&c, &d, 1).unwrap(),
            json!({"data": {"color": "b", "k": "v"}})
        );
        assert_eq!(
            typed_answer(&c, "red").unwrap(),
            json!({"data": {"color": "r", "k": "v"}})
        );
        assert!(typed_answer(&c, "green").is_err());
    }

    /// DISC-OUT req. 63, 70, 72: a form with several fields is posted with its fields and answered by
    /// a JSON map; omitted fields keep their defaults, hidden ones are returned unchanged.
    #[test]
    fn forms_with_several_fields_take_a_json_map() {
        let html = r#"<form><p>Release details</p><fieldset><legend>Channel</legend><label><input type="radio" name="channel" value="s" checked> Stable</label><label><input type="radio" name="channel" value="b"> Beta</label></fieldset><label>Note <input type="text" name="note" required></label><input type="hidden" name="k" value="v"></form>"#;
        let d = dialog_of(&json!({"id": "f", "method": "form", "html": html})).unwrap();
        let c = card(&d);
        assert!(matches!(c.kind, Kind::Fields { .. }), "{:?}", c.kind);
        assert_eq!(c.choices, Vec::<String>::new(), "no buttons");
        assert!(
            c.body.contains("- channel")
                && c.body.contains("s = Stable")
                && c.body.contains("- note"),
            "{}",
            c.body
        );
        assert!(answer_of(&c, &d, 0).is_none());
        assert_eq!(
            typed_answer(&c, r#"{"note": "ship it"}"#).unwrap(),
            json!({"data": {"channel": "s", "note": "ship it", "k": "v"}})
        );
        assert_eq!(
            typed_answer(&c, r#"{"channel": "beta", "note": "x"}"#).unwrap()["data"]["channel"],
            "b"
        );
        assert!(
            typed_answer(&c, r#"{"channel": "s"}"#).is_err(),
            "note is required"
        );
        assert!(
            typed_answer(&c, r#"{"k": "w", "note": "x"}"#).is_err(),
            "hidden"
        );
        assert!(typed_answer(&c, r#"{"channel": "nightly", "note": "x"}"#).is_err());
        assert!(typed_answer(&c, "beta").is_err());
    }

    /// An `ask` with several questions is posted as one card and answered by a JSON map.
    #[test]
    fn ask_with_several_questions_takes_a_json_map() {
        let d = dialog_of(&json!({"type": "extension_ui_request", "id": "d2", "method": "ask",
            "questions": [
                {"id": "color", "question": "Pick", "options": [{"label": "Red"}, {"label": "Blue"}], "recommended": 1},
                {"id": "size", "question": "Size", "multi": true, "options": [{"label": "S"}, {"label": "M"}]}
            ]}))
        .unwrap();
        let c = card(&d);
        assert!(matches!(c.kind, Kind::Asks { .. }), "{:?}", c.kind);
        assert!(
            c.body.contains("- color") && c.body.contains("default: Blue"),
            "{}",
            c.body
        );
        assert_eq!(
            typed_answer(&c, r#"{"size": ["s", "M"]}"#).unwrap(),
            json!({"answers": [
                {"id": "color", "selectedOptions": ["Blue"]},
                {"id": "size", "selectedOptions": ["S", "M"]}
            ]})
        );
        assert_eq!(
            typed_answer(&c, r#"{"color": "green", "size": "M"}"#).unwrap()["answers"][0]["customInput"],
            "green"
        );
        assert!(
            typed_answer(&c, r#"{"color": "Red"}"#).is_err(),
            "size unanswered"
        );
        assert!(typed_answer(&c, r#"{"shape": "x", "size": "M"}"#).is_err());
    }

    #[test]
    fn questions_convert_and_answers_validate() {
        let d = ask(Some(1), false);
        let c = card(&d);
        assert_eq!(card_of(&d, false).unwrap().3, Some(1));
        assert!(c.body.contains("@\u{200b}everyone") && c.body.contains("\\`x\\`"));
        assert_eq!(
            answer_of(&c, &d, 1).unwrap(),
            json!({"answers": [{"id": "q1", "selectedOptions": ["Blue"]}]})
        );
        assert_eq!(
            typed_answer(&c, "1").unwrap()["answers"][0]["selectedOptions"][0],
            "Red"
        );
        assert_eq!(
            typed_answer(&c, "blue").unwrap()["answers"][0]["selectedOptions"][0],
            "Blue"
        );
        assert_eq!(
            typed_answer(&c, "green please").unwrap()["answers"][0]["customInput"],
            "green please"
        );
        // A multi-select question has no recommended default.
        assert_eq!(card_of(&ask(Some(1), true), false).unwrap().3, None);
        let b = buttons("s", &c.choices, Some(1), false);
        assert_eq!(b[0]["components"][1]["style"], 1);
        assert_eq!(b[0]["components"][0]["custom_id"], "cy:s:0");
        // Approvals stay local unless remote approvals are on.
        let approval = dialog_of(&json!({"id": "a", "method": "select", "title": "Run bash?",
            "options": ["Approve Once", "Deny"]}))
        .unwrap();
        assert!(card_of(&approval, false).is_none());
        let (kind, ..) = card_of(&approval, true).unwrap();
        assert_eq!(kind, Kind::Approval);
        let confirm = dialog_of(&json!({"id": "c", "method": "confirm", "title": "Go?"})).unwrap();
        let mut cc = card(&confirm);
        cc.kind = Kind::Confirm;
        assert_eq!(typed_answer(&cc, "2").unwrap(), json!({"confirmed": false}));
        assert!(typed_answer(&cc, "maybe").is_err());
    }
}
