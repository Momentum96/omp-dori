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
use crate::config::Language;
use crate::host::dialogs::DialogKind;
use crate::host::dialogs::Question;
use crate::host::{Dialog, Runtime};
use crate::log;
use crate::relay::Inbound;
use crate::relay::text::DISCORD_LIMIT;
use crate::store::relay::{QueueRow, RelayWrite};

mod approval;
mod panel;
mod typed;
/// The longest excerpt of a waiting dialog in its local notice.
const NOTICE_DETAIL: usize = 300;
/// The question summary may be at most this long (DISC-OUT req. 63).
const SUMMARY_MAX: usize = 1500;
/// Buttons at most: 5 rows of 5 (DISC-OUT req. 67).
const BUTTONS_MAX: usize = 25;
const LABEL_MAX: usize = 80;

/// The suffix after the option OMP marks recommended, in the owner's language.
fn recommended_mark(language: Language) -> &'static str {
    match language {
        Language::Korean => " (추천)",
        Language::English => " (recommended)",
    }
}

/// `label` followed by `mark`, the label cut so the whole stays within `max` characters; the mark
/// is never cut.
fn marked(label: &str, mark: &str, max: usize) -> String {
    let room = max.saturating_sub(mark.chars().count());
    let mut out: String = label.chars().take(room).collect();
    out.push_str(mark);
    out
}

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
        limits: super::form::Limits,
    },
    /// An HTML form with several editable fields: select menus, a modal for text and Submit
    /// (`panel`); hidden fields are returned unchanged.
    Fields {
        fields: Vec<Field>,
        hidden: serde_json::Map<String, Value>,
    },
    /// The `ask` tool with several questions, answered like a form with several fields.
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
    /// The fields of a `Fields` or `Asks` card and the owner's answers so far.
    items: Option<Vec<panel::Item>>,
    draft: Vec<panel::Val>,
    /// Further messages of a card that one message cannot hold.
    extra: Vec<Snowflake>,
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

    /// Forgets local-only dialogs that are no longer open, so the set holds only open dialogs.
    fn keep_skipped(&self, live: &HashSet<&str>) {
        self.lock().skipped.retain(|d| live.contains(d.as_str()));
    }

    fn by_message(&self, message: Snowflake) -> Option<Card> {
        self.lock()
            .open
            .values()
            .find(|c| c.message == message || c.extra.contains(&message))
            .cloned()
    }
}

/// Keeps text from mentioning anyone, opening code spans or forming Markdown (DISC-OUT req. 63).
pub(crate) fn neutral(text: &str) -> String {
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

/// The card for a dialog, or `None` when it stays local (DISC-OUT req. 59-64). `approvals` is the
/// owner's language when remote approvals are on; an approval card is written in it. `language`
/// is the owner's language for the recommended option's mark.
fn card_of(
    dialog: &Dialog,
    language: Language,
    approvals: Option<Language>,
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
                return asks_card(dialog.questions.as_deref()?, language);
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
                    limits: field.limits.clone(),
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
        DialogKind::Approval if !dialog.secret => {
            let (text, buttons) = approval::card(dialog, approvals?);
            Some((Kind::Approval, text, buttons, None))
        }
        _ => None,
    }
}

/// The notice for a dialog that waits on the machine: what it is, why it is not here, and the
/// command that answers it there.
fn local_notice(dialog: &Dialog, remote_approvals: bool) -> String {
    let approval = dialog.kind == DialogKind::Approval;
    let what = if approval {
        "a permission request from the lead"
    } else {
        "a question from the lead"
    };
    let why = if dialog.secret {
        "it asks for a secret"
    } else if approval && !remote_approvals {
        "remote approvals are off"
    } else {
        "it cannot be answered here"
    };
    let raw: Vec<&str> = [&dialog.title, &dialog.message]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let raw = raw.join(" ");
    let mut detail: String = raw.chars().take(NOTICE_DETAIL).collect();
    if detail.len() < raw.len() {
        detail.push('…');
    }
    let mut text = format!("**Waiting on the machine:** {what}");
    if !detail.is_empty() {
        let _ = write!(text, ": {}", neutral(&detail));
    }
    let _ = write!(text, " ({why}). Answer it there: ");
    let id = &dialog.id;
    let plain_id = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    if !plain_id {
        text.push_str("`cataly lead status` shows its ID, `cataly lead answer` answers it.");
    } else if approval {
        let has = |o: &str| dialog.options.iter().flatten().any(|x| x == o);
        let approve = if has("Approve Once") {
            "Approve Once"
        } else {
            "Approve"
        };
        let _ = write!(
            text,
            "`cataly lead answer {id} '{{\"value\":\"{approve}\"}}'` or `cataly lead answer {id} '{{\"value\":\"Deny\"}}'`."
        );
    } else {
        let _ = write!(
            text,
            "`cataly lead answer {id} '<answer as JSON>'`; `cataly lead status` shows what it asks."
        );
    }
    text
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

/// An `ask` dialog with several questions: each with its ID, choices and recommended choice.
fn asks_card(
    questions: &[Question],
    language: Language,
) -> Option<(Kind, String, Vec<String>, Option<usize>)> {
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
            let _ = write!(
                text,
                "; default: {}{}",
                neutral(r),
                recommended_mark(language)
            );
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

fn form_data(field: &str, hidden: &serde_json::Map<String, Value>, value: Value) -> Value {
    let mut data = hidden.clone();
    data.insert(field.to_string(), value);
    json!({ "data": data })
}

/// The index of a typed choice: an option number from 1, or a unique label in any ASCII case.
fn pick(choices: &[String], item: &str) -> Option<usize> {
    let item = item.trim();
    if let Ok(n) = item.parse::<usize>() {
        return (1..=choices.len()).contains(&n).then(|| n - 1);
    }
    let hits: Vec<usize> = choices
        .iter()
        .enumerate()
        .filter(|(_, c)| c.eq_ignore_ascii_case(item))
        .map(|(i, _)| i)
        .collect();
    match hits.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// A typed answer (DISC-OUT req. 70, 72) to a card with one part: an option number, a unique
/// label, or for `ask` free text. A card with several parts takes [`typed::part_value`].
fn typed_answer(card: &Card, text: &str, words: &typed::Words) -> Result<Value, String> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > 2000 {
        return Err(words.length.into());
    }
    let pick = |item: &str| pick(&card.choices, item);
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
        Kind::Fields { .. } | Kind::Asks { .. } => Err(words.which.into()),
        Kind::Form {
            field,
            values,
            hidden,
            free,
            multi,
            limits,
        } => {
            if *free {
                words.check(&neutral(field), limits, text)?;
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
                None => Err(words.number(card.choices.len())),
            }
        }
        Kind::Select | Kind::Confirm => match pick(text) {
            Some(i) if card.kind == Kind::Select => Ok(json!({ "value": card.choices.get(i) })),
            Some(i) => Ok(json!({ "confirmed": i == 0 })),
            None => Err(words.number(card.choices.len())),
        },
        Kind::Approval => Err("approvals are answered with the buttons".into()),
    }
}

fn buttons(
    stamp: &str,
    choices: &[String],
    recommended: Option<usize>,
    approval: bool,
    mark: &str,
) -> Value {
    let row_of = |chunk: &[(usize, &String)]| {
        let components: Vec<Value> = chunk
            .iter()
            .map(|(i, label)| {
                let label = if !approval && Some(*i) == recommended {
                    marked(label, mark, LABEL_MAX)
                } else if label.chars().count() > LABEL_MAX {
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

fn choice_lines(choices: &[String], recommended: Option<usize>, mark: &str) -> String {
    choices
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let tag = if Some(i) == recommended { mark } else { "" };
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
        self.mark_question(channel, message, false).await;
    }

    /// Records a posted question as open, or closes its record (DISC-OUT req. 76).
    async fn mark_question(&self, channel: Snowflake, message: Snowflake, open: bool) {
        let write = RelayWrite::Question {
            relay: NAME.into(),
            room: channel.to_string(),
            message: message.to_string(),
            open,
        };
        if let Err(e) = self.store.relay(write).await {
            let what = if open {
                "question not recorded"
            } else {
                "question record not closed"
            };
            log::warn(what, &[("error", &e)]);
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
        let snapshot = self.lead.snapshot();
        let dialogs: Vec<Dialog> = snapshot
            .as_ref()
            .map_or_else(Vec::new, |s| s.dialogs.clone());
        let live: HashSet<&str> = dialogs.iter().map(|d| d.id.as_str()).collect();
        if snapshot.is_some() {
            self.cards.keep_skipped(&live);
        }
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
                approval::words(self.language).run_ended
            } else {
                "Closed: answered elsewhere."
            };
            self.close(&card, &format!("{}\n\n{status}", card.body))
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
        let mut body = if choices.is_empty() || *kind == Kind::Approval {
            // An approval is answered with its buttons only.
            head.to_string()
        } else {
            format!(
                "{head}\n{}",
                choice_lines(choices, recommended, recommended_mark(self.language))
            )
        };
        let words = typed::words(self.language);
        match kind {
            Kind::Approval => {}
            Kind::Fields { .. } | Kind::Asks { .. } => {
                let _ = write!(body, "\n{}", words.panel_hint);
            }
            _ => {
                let _ = write!(body, "\n{}", words.hint);
            }
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
    /// Returns its ID; `None` (logged) when it was not posted.
    async fn first_message(
        &self,
        dialog: &Dialog,
        channel: Snowflake,
        reply_to: Option<Snowflake>,
        body: &str,
        stamp: &str,
        components: Option<&Value>,
    ) -> Option<Snowflake> {
        let approval = dialog.kind == DialogKind::Approval;
        let posted = if approval {
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
        };
        match posted {
            Ok(v) => v
                .get("id")
                .and_then(Value::as_str)
                .and_then(Snowflake::parse),
            Err(e) => {
                log::warn("question not posted; it stays local", &[("error", &e)]);
                None
            }
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

    /// Marks a dialog local only and says so once, in the room of the request that started the run
    /// (else the home channel): what waits and how to answer it on the machine (review 6). A dialog
    /// left local without a notice would block the run with nothing said in the chat.
    async fn stay_local(&self, dialog: &Dialog) {
        if !self.cards.lock().skipped.insert(dialog.id.clone()) {
            return;
        }
        let room = match self.route(true).await {
            Some(r) => Some(r),
            None => self.settings.home().map(|c| (c, None)),
        };
        let Some((channel, reply_to)) = room else {
            return;
        };
        let mut create = json!({
            "content": local_notice(dialog, self.settings.remote_approvals),
            "nonce": nonce(&format!("local:{}:{}", self.bot, dialog.id), 0),
        });
        if let Some(source) = reply_to {
            super::put(
                &mut create,
                "message_reference",
                json!({"message_id": source.to_string(),
                "channel_id": channel.to_string(), "fail_if_not_exists": false}),
            );
        }
        if let Err(e) = self.rest.create(channel, create).await {
            log::warn("local dialog notice not posted", &[("error", &e)]);
        }
    }

    /// Posts one dialog, or marks it local only.
    async fn post_card(self: &Arc<Self>, dialog: &Dialog) {
        let skip = |me: &Self| {
            me.cards.lock().skipped.insert(dialog.id.clone());
        };
        let lang = self.language;
        let approvals = self.settings.remote_approvals.then_some(lang);
        let Some((kind, head, choices, recommended)) = card_of(dialog, lang, approvals) else {
            return self.stay_local(dialog).await;
        };
        let mark = recommended_mark(lang);
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
                return skip(self);
            }
            return self.stay_local(dialog).await;
        }
        let panel = matches!(kind, Kind::Fields { .. } | Kind::Asks { .. });
        let multi = matches!(
            kind,
            Kind::Ask { multi: true, .. }
                | Kind::Form { multi: true, .. }
                | Kind::Form { free: true, .. }
        ) || panel;
        let stamp = self.stamp(approval, reply_to, &dialog.id);
        let items = match &kind {
            Kind::Fields { fields, .. } => panel::of_fields(fields),
            Kind::Asks { questions } => panel::of_questions(questions),
            _ => None,
        };
        let layout = |i: &[panel::Item]| panel::layout(&stamp, i, mark);
        let mut pages = items.as_deref().and_then(layout).unwrap_or_default();
        if panel && (!self.buttons || pages.is_empty()) {
            // Without components such a question cannot be answered on Discord; it stays local.
            return self.stay_local(dialog).await;
        }
        let components = if panel {
            Some(pages.remove(0))
        } else {
            (self.buttons && !multi && body.encode_utf16().count() <= DISCORD_LIMIT)
                .then(|| buttons(&stamp, &choices, recommended, approval, mark))
        };
        // The card exists before its buttons can be seen, so no press can find it missing.
        let message = self
            .first_message(
                dialog,
                channel,
                reply_to,
                &body,
                &stamp,
                components.as_ref(),
            )
            .await;
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
            stamp: stamp.clone(),
            state: State::Open,
            draft: items.as_deref().map_or_default(panel::draft),
            items,
            extra: Vec::new(),
        };
        self.cards.lock().open.insert(dialog.id.clone(), card);
        self.mark_question(channel, message, true).await;
        if !approval
            && !self
                .show(&dialog.id, channel, message, &body, components)
                .await
        {
            return;
        }
        self.post_parts(&dialog.id, channel, &stamp, pages).await;
        if let Some(index) = recommended {
            let me = Arc::clone(self);
            let id = dialog.id.clone();
            tokio::spawn(async move { me.recommended_after(&id, index).await });
        }
    }

    /// Posts the second and later messages of a card too large for one, each with its rows.
    async fn post_parts(&self, dialog: &str, channel: Snowflake, stamp: &str, pages: Vec<Value>) {
        for (n, rows) in pages.into_iter().enumerate() {
            let seed = format!("question:{}:{dialog}:{stamp}:{}", self.bot, n + 1);
            let part = json!({"content": format!("Part {} of the question above.", n + 2),
                "nonce": nonce(&seed, 0), "components": rows});
            match self.rest.create(channel, part).await {
                Ok(v) => {
                    if let Some(id) = v
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(Snowflake::parse)
                        && let Some(c) = self.cards.lock().open.get_mut(dialog)
                    {
                        c.extra.push(id);
                    }
                }
                Err(e) => log::warn("question part not posted", &[("error", &e)]),
            }
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

    /// A press on the ops panel: the owner's is answered by the panel, anyone else's refused.
    async fn ops_press(&self, press: &Press) {
        if !press.by_owner(self.application, self.owner) {
            let lang = self.jobs.ctx().config.borrow().hub.language;
            return self.refuse(press, crate::jobs::ops::refusal(lang)).await;
        }
        let body = self.jobs.ops_press(&press.custom_id, press.channel).await;
        if let Err(e) = self.rest.callback(&press.id, &press.token, body).await {
            log::info("ops panel press not answered", &[("error", &e)]);
        }
    }

    async fn refuse(&self, press: &Press, text: &str) {
        let body = json!({"type": 4, "data": {"content": text, "flags": 64}});
        if let Err(e) = self.rest.callback(&press.id, &press.token, body).await {
            log::info("refusal not sent", &[("error", &e)]);
        }
    }

    /// A button press (DISC-OUT req. 69, 82): the owner's press on an open card answers it; anyone
    /// else's gets an ephemeral refusal; presses while one is being taken are ignored. Presses on
    /// the ops panel go to the panel.
    pub(super) async fn press(self: Arc<Self>, press: Press) {
        if press.custom_id.starts_with("ops:") {
            return self.ops_press(&press).await;
        }
        let Some(card) = self.cards.by_message(press.message) else {
            return self.refuse(&press, "This question is closed.").await;
        };
        if !press.by_owner(self.application, self.owner) || press.channel != card.channel {
            return self.refuse(&press, "Only the owner can answer this.").await;
        }
        let Some(action) = press
            .custom_id
            .strip_prefix(&format!("cy:{}:", card.stamp))
            .map(str::to_string)
        else {
            return self
                .refuse(&press, "This button is not part of the question.")
                .await;
        };
        let Some(index) = action
            .parse::<usize>()
            .ok()
            .filter(|i| *i < card.choices.len())
        else {
            return self.panel_press(card, &action, press).await;
        };
        let Some(card) = self.cards.take(&card.dialog) else {
            return;
        };
        let label = card.choices.get(index).cloned().unwrap_or_default();
        let done = if card.kind == Kind::Approval {
            let [approved, denied] = approval::words(self.language).answered;
            format!(
                "{}\n\n{}",
                card.body,
                if index == 0 { approved } else { denied }
            )
        } else {
            format!(
                "{}\n\nAnswered by the owner: {}",
                card.body,
                neutral(&label)
            )
        };
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
        if let Some(answer) = live.as_ref().and_then(|d| answer_of(&card, d, index)) {
            self.submit_value(card, answer, done).await;
        } else {
            self.cards.remove(&card.dialog);
            let content = format!("{}\n\nClosed: the question was already answered", card.body);
            self.close(&card, &content).await;
        }
    }

    /// Submits an answer of a taken card and closes the card.
    async fn submit_value(&self, card: Card, answer: Value, done: String) {
        let live = self
            .lead
            .snapshot()
            .is_some_and(|s| s.dialogs.iter().any(|d| d.id == card.dialog));
        let outcome = if live {
            self.lead.answer(&card.dialog, &answer).await
        } else {
            Err("the question was already answered".into())
        };
        self.cards.remove(&card.dialog);
        let content = match outcome {
            Ok(()) => done,
            Err(e) => format!("{}\n\nClosed: {}", card.body, neutral(&e)),
        };
        self.close(&card, &content).await;
    }

    /// Edits a card's question into its final text and clears the components of its other parts.
    async fn close(&self, card: &Card, content: &str) {
        self.edit_closing(card.channel, card.message, content).await;
        for part in &card.extra {
            let body = json!({ "content": "Closed.", "components": [] });
            if let Err(e) =
                tokio::time::timeout(EDIT_BUDGET, self.rest.edit(card.channel, *part, body)).await
            {
                log::info("question part not updated within 3 s", &[("error", &e)]);
            }
        }
    }

    /// An owner message that answers an open question (DISC-OUT req. 70-73): a reply to the question
    /// (or to one part of it), or a plain message in its channel when exactly one question is open
    /// there. On a card with several parts it answers one part; the others stay open.
    pub(super) async fn answer_by_text(&self, row: &QueueRow, inbound: &Inbound) -> bool {
        let words = typed::words(self.language);
        let target = {
            let inner = self.cards.lock();
            let here: Vec<&Card> = inner
                .open
                .values()
                .filter(|c| c.kind != Kind::Approval && c.channel.to_string() == row.room)
                .collect();
            let to = inbound.reply_to.as_deref();
            let replied = here.iter().find_map(|c| {
                let to = to?;
                let parts = c.items.as_ref().map_or(0, Vec::len);
                if c.message.to_string() == to {
                    // A card split into one message per part starts with part 1.
                    let split = !c.extra.is_empty() && c.extra.len() + 1 == parts;
                    return Some((*c, split.then_some(0)));
                }
                let k = c.extra.iter().position(|m| m.to_string() == to)?;
                Some((*c, (c.extra.len() + 1 == parts).then_some(k + 1)))
            });
            match (replied, here.as_slice()) {
                (Some((c, part)), _) => Some(((*c).clone(), part, true)),
                (None, [one]) if inbound.plain && row.seq > one.message.seq() => {
                    Some(((*one).clone(), None, false))
                }
                _ => None,
            }
        };
        let Some((card, part, reply)) = target else {
            return false;
        };
        if card.items.is_some() {
            return self
                .answer_part(row, inbound, &card, part, reply, &words)
                .await;
        }
        let Some(card) = self.cards.take(&card.dialog) else {
            return false;
        };
        let answer = match typed_answer(&card, &inbound.text, &words) {
            Ok(a) => a,
            Err(problem) => {
                self.cards.reopen(&card.dialog);
                self.correct(&card, row, &words.retry(&problem)).await;
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
            Ok(()) => format!("{}\n\n{}", card.body, words.answered),
            Err(e) => format!("{}\n\nClosed: {}", card.body, neutral(&e)),
        };
        self.edit_closing(card.channel, card.message, &content)
            .await;
        true
    }

    /// A typed answer to one part of a card with several questions or fields: the part replied to,
    /// or the only part still open. The card is submitted once every part has an answer that
    /// checks; otherwise the owner hears which part was taken. A plain message that names no part
    /// is not an answer.
    async fn answer_part(
        &self,
        row: &QueueRow,
        inbound: &Inbound,
        card: &Card,
        part: Option<usize>,
        reply: bool,
        words: &typed::Words,
    ) -> bool {
        let Some(n) = typed::part_of(card, part) else {
            if reply {
                self.correct(card, row, &words.retry(words.which)).await;
            }
            return reply;
        };
        let val = match typed::part_value(card, n, &inbound.text, words) {
            Ok(v) => v,
            Err(problem) => {
                self.correct(card, row, &words.retry(&problem)).await;
                return true;
            }
        };
        self.cards.set(&card.dialog, n, val);
        let items = card.items.clone().unwrap_or_default();
        let draft = self.cards.draft(&card.dialog);
        let ready = !draft.contains(&panel::Val::Unset)
            && panel::answer(&card.kind, &items, &draft, words).is_ok();
        if !ready {
            let name = items.get(n).map_or("", panel::label);
            self.correct(card, row, &words.recorded(name)).await;
            return true;
        }
        let Some(card) = self.cards.take(&card.dialog) else {
            return true;
        };
        match panel::answer(&card.kind, &items, &card.draft, words) {
            Ok(value) => {
                let done = format!("{}\n\n{}", card.body, words.answered);
                self.submit_value(card, value, done).await;
            }
            Err(_) => self.cards.reopen(&card.dialog),
        }
        true
    }

    /// Replies to the owner's message about a typed answer (not taken, or one part taken).
    async fn correct(&self, card: &Card, row: &QueueRow, text: &str) {
        let seed = format!("correction:{}:{}", self.bot, row.message);
        if let crate::relay::PartOutcome::Rejected(e) | crate::relay::PartOutcome::Uncertain(e) =
            self.rest
                .reply_part(
                    card.channel,
                    Snowflake::parse(&row.message),
                    text,
                    &nonce(&seed, 0),
                )
                .await
        {
            log::info("correction not posted", &[("error", &e)]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::dialogs::dialog_of;

    fn typed_answer(card: &Card, text: &str) -> Result<Value, String> {
        super::typed_answer(card, text, &typed::words(Language::English))
    }

    /// Local-only dialogs are remembered while open and forgotten once gone.
    #[test]
    fn local_only_dialogs_are_forgotten_when_gone() {
        let cards = Cards::default();
        for n in 0..1000 {
            cards.lock().skipped.insert(format!("d{n}"));
            cards.keep_skipped(&HashSet::from([format!("d{n}").as_str()]));
        }
        assert_eq!(cards.lock().skipped.len(), 1);
        assert!(cards.lock().skipped.contains("d999"));
        cards.keep_skipped(&HashSet::new());
        assert!(cards.lock().skipped.is_empty());
    }

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
        let (kind, body, choices, _) = card_of(dialog, Language::English, None).unwrap();
        let items = match &kind {
            Kind::Fields { fields, .. } => panel::of_fields(fields),
            Kind::Asks { questions } => panel::of_questions(questions),
            _ => None,
        };
        Card {
            dialog: dialog.id.clone(),
            channel: Snowflake::parse("7").unwrap(),
            message: Snowflake::parse("9").unwrap(),
            kind,
            choices,
            body,
            stamp: "s".into(),
            state: State::Open,
            draft: items.as_deref().map_or_default(panel::draft),
            items,
            extra: Vec::new(),
        }
    }

    #[test]
    fn omp_forms_become_questions() {
        let html = r#"<form><fieldset><legend>Pick a color</legend><label><input type="radio" name="color" value="r"> Red</label><label><input type="radio" name="color" value="b" checked> Blue</label></fieldset><input type="hidden" name="k" value="v"></form>"#;
        let d = dialog_of(&json!({"id": "f", "method": "form", "html": html})).unwrap();
        let c = card(&d);
        assert_eq!(c.choices, vec!["Red", "Blue"]);
        assert_eq!(card_of(&d, Language::English, None).unwrap().3, Some(1));
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

    /// DISC-OUT req. 63, 72: a form with several fields gets a select per choice field, a modal for
    /// its text fields and Submit; ranges, lengths and required fields are checked; the owner never
    /// types JSON.
    #[test]
    fn forms_with_several_fields_use_menus_and_a_modal() {
        let html = r#"<form><p>Release details</p><fieldset><legend>Channel</legend><label><input type="radio" name="channel" value="s" checked> Stable</label><label><input type="radio" name="channel" value="b"> Beta</label></fieldset><label>Note <input type="text" name="note" required maxlength="5"></label><label>Count <input type="number" name="count" min="1" max="3"></label><input type="hidden" name="k" value="v"></form>"#;
        let d = dialog_of(&json!({"id": "f", "method": "form", "html": html})).unwrap();
        let mut c = card(&d);
        assert!(matches!(c.kind, Kind::Fields { .. }), "{:?}", c.kind);
        assert!(
            typed_answer(&c, r#"{"note": "x"}"#).is_err(),
            "no JSON answers"
        );
        let items = c.items.clone().unwrap();
        let pages = panel::layout("s", &items, " (recommended)").unwrap();
        assert_eq!(
            pages.len(),
            1,
            "one message holds one select and one button row"
        );
        let rows = pages[0].as_array().unwrap();
        assert_eq!(rows[0]["components"][0]["type"], 3);
        assert_eq!(rows[0]["components"][0]["custom_id"], "cy:s:s:0");
        assert_eq!(rows[1]["components"][0]["custom_id"], "cy:s:m:1-2");
        assert_eq!(rows[1]["components"][1]["custom_id"], "cy:s:go");
        let (en, ko) = (
            typed::words(Language::English),
            typed::words(Language::Korean),
        );
        let both =
            |c: &Card| [&en, &ko].map(|w| panel::answer(&c.kind, &items, &c.draft, w).unwrap_err());
        assert_eq!(
            both(&c),
            [vec!["Note is required"], vec!["Note: 꼭 답해 주세요"]]
        );
        c.draft[1] = panel::Val::Text("toolong".into());
        assert_eq!(
            both(&c),
            [
                vec!["Note takes 0 to 5 characters"],
                vec!["Note: 0자에서 5자까지 써 주세요"]
            ]
        );
        c.draft[1] = panel::Val::Text("ship".into());
        c.draft[2] = panel::Val::Text("9".into());
        assert_eq!(
            both(&c),
            [
                vec!["Count takes a number from 1 to 3"],
                vec!["Count: 1부터 3까지의 숫자로 답해 주세요"]
            ]
        );
        c.draft[2] = panel::Val::Text("x".into());
        let [en_problems, ko_problems] = both(&c);
        assert_eq!(
            [
                en.not_submitted(&en_problems),
                ko.not_submitted(&ko_problems)
            ],
            [
                "Not submitted: Count takes a number.",
                "제출하지 못했어요: Count: 숫자로 답해 주세요."
            ]
        );
        c.draft[2] = panel::Val::Text("2".into());
        c.draft[0] = panel::Val::Picks(vec![1]);
        assert_eq!(
            panel::answer(&c.kind, &items, &c.draft, &ko).unwrap(),
            json!({"data": {"channel": "b", "note": "ship", "count": "2", "k": "v"}})
        );
    }

    /// The modal's number placeholder and a refused Fill in follow `[hub] language`, open bounds
    /// included.
    #[test]
    fn field_limit_texts_follow_the_owners_language() {
        let (en, ko) = (
            typed::words(Language::English),
            typed::words(Language::Korean),
        );
        let limits = |min: Option<&str>, max: Option<&str>| super::super::form::Limits {
            number: true,
            min: min.map(String::from),
            max: max.map(String::from),
            ..Default::default()
        };
        let ranges = |w: &typed::Words| {
            [
                w.range(&limits(Some("1"), Some("3"))),
                w.range(&limits(Some("2"), None)),
                w.range(&limits(None, Some("9"))),
                w.range(&limits(None, None)),
            ]
        };
        assert_eq!(
            ranges(&en),
            [
                "a number from 1 to 3",
                "a number from 2 to any",
                "a number from any to 9",
                "a number from any to any"
            ]
        );
        assert_eq!(
            ranges(&ko),
            [
                "1부터 3까지의 숫자",
                "2 이상의 숫자",
                "9 이하의 숫자",
                "숫자"
            ]
        );
        let text = super::super::form::Limits {
            min_len: Some(2),
            ..Default::default()
        };
        assert_eq!(
            en.check("Name", &text, "a"),
            Err("Name takes 2 to any number of characters".into())
        );
        assert_eq!(
            ko.check("Name", &text, "a"),
            Err("Name: 2자 이상 써 주세요".into())
        );
        assert_eq!(ko.check("Name", &text, "ab"), Ok(()));
        let problems = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            en.not_taken(&problems),
            "Not taken: a; b. Use Fill in again."
        );
        assert_eq!(
            ko.not_taken(&problems),
            "받지 못했어요: a; b. Fill in을 다시 눌러 주세요."
        );
    }

    /// An `ask` with several questions: one select per question, one message per question when one
    /// message cannot hold them, Submit on the last.
    #[test]
    fn ask_with_several_questions_uses_menus() {
        let q = |id: &str| json!({"id": id, "question": id, "options": [{"label": "A"}, {"label": "B"}]});
        let d = dialog_of(&json!({"type": "extension_ui_request", "id": "d2", "method": "ask",
            "questions": [q("a"), q("b"), q("c"), q("d"), q("e"), {"id": "f", "question": "Why?", "options": []}]}))
        .unwrap();
        let mut c = card(&d);
        assert!(matches!(c.kind, Kind::Asks { .. }), "{:?}", c.kind);
        let items = c.items.clone().unwrap();
        let pages = panel::layout("s", &items, " (recommended)").unwrap();
        assert_eq!(pages.len(), 6, "one message per question");
        assert_eq!(pages[5][0]["components"][0]["custom_id"], "cy:s:m:5");
        assert_eq!(pages[5][1]["components"][0]["custom_id"], "cy:s:go");
        for (n, slot) in c.draft.iter_mut().take(5).enumerate() {
            *slot = panel::Val::Picks(vec![n % 2]);
        }
        assert!(
            panel::answer(&c.kind, &items, &c.draft, &typed::words(Language::English)).is_err(),
            "f unanswered"
        );
        c.draft[5] = panel::Val::Text("because".into());
        let a = panel::answer(&c.kind, &items, &c.draft, &typed::words(Language::English)).unwrap();
        assert_eq!(
            a["answers"][1],
            json!({"id": "b", "selectedOptions": ["B"]})
        );
        assert_eq!(a["answers"][5]["customInput"], "because");
    }

    /// The texts around typed answers follow `[hub] language`.
    #[test]
    fn typed_answer_texts_follow_the_owners_language() {
        let ko = typed::words(Language::Korean);
        assert_eq!(
            ko.hint,
            "번호를 누르거나, 원하는 답을 채팅으로 보내 주세요."
        );
        assert_eq!(ko.answered, "보스가 채팅으로 답했습니다.");
        assert_eq!(
            ko.retry("x"),
            "그 답은 받을 수 없어요: x. 다시 보내 주세요."
        );
        assert_eq!(ko.number(3), "1부터 3까지의 번호로 답해 주세요");
        let en = typed::words(Language::English);
        assert_eq!(
            en.hint,
            "Reply to this message with a number or your answer."
        );
        assert_eq!(
            en.retry("x"),
            "That answer was not taken: x. Please try again."
        );
        let confirm = dialog_of(&json!({"id": "c", "method": "confirm", "title": "Go?"})).unwrap();
        let mut cc = card(&confirm);
        cc.kind = Kind::Confirm;
        assert_eq!(super::typed_answer(&cc, "maybe", &ko), Err(ko.number(2)));
        assert_eq!(
            super::typed_answer(&cc, " ", &ko),
            Err(ko.length.to_string())
        );
    }

    /// A typed reply to one part of a card with several questions or fields answers that part; the
    /// others stay open. Without a part named, only the single open part can be answered.
    #[test]
    fn a_typed_reply_answers_one_part() {
        let en = typed::words(Language::English);
        let question = |id: &str| json!({"id": id, "question": id, "options": [{"label": "A"}, {"label": "B"}]});
        let dialog = dialog_of(&json!({"id": "d3", "method": "ask", "questions": [question("a"), question("b"), {"id": "c", "question": "Why?", "options": []}]}))
        .unwrap();
        let mut c = card(&dialog);
        assert_eq!(typed::part_of(&c, None), None, "three parts open");
        assert_eq!(typed::part_of(&c, Some(1)), Some(1));
        assert_eq!(
            typed::part_value(&c, 1, "2", &en),
            Ok(panel::Val::Picks(vec![1]))
        );
        assert_eq!(
            typed::part_value(&c, 0, "b", &en),
            Ok(panel::Val::Picks(vec![1]))
        );
        // OMP takes custom input for an `ask` question.
        assert_eq!(
            typed::part_value(&c, 0, "neither", &en),
            Ok(panel::Val::Text("neither".into()))
        );
        assert_eq!(
            typed::part_value(&c, 2, "because", &en),
            Ok(panel::Val::Text("because".into()))
        );
        c.draft[0] = panel::Val::Picks(vec![0]);
        c.draft[1] = panel::Val::Picks(vec![1]);
        assert_eq!(typed::part_of(&c, None), Some(2), "the only open part");
        c.draft[2] = panel::Val::Text("because".into());
        let a = panel::answer(&c.kind, c.items.as_deref().unwrap(), &c.draft, &en).unwrap();
        assert_eq!(a["answers"][2]["customInput"], "because");
        // A form's choice field takes only its options.
        let html = r#"<form><p>Release</p><fieldset><legend>Channel</legend><label><input type="radio" name="channel" value="s"> Stable</label><label><input type="radio" name="channel" value="b"> Beta</label></fieldset><label>Note <input type="text" name="note" maxlength="5"></label></form>"#;
        let f = card(&dialog_of(&json!({"id": "f", "method": "form", "html": html})).unwrap());
        assert_eq!(
            typed::part_value(&f, 0, "beta", &en),
            Ok(panel::Val::Picks(vec![1]))
        );
        assert_eq!(typed::part_value(&f, 0, "gamma", &en), Err(en.number(2)));
        assert!(typed::part_value(&f, 1, "too long", &en).is_err());
        assert_eq!(
            en.recorded("Channel"),
            "Taken for Channel; fill in the rest, then press Submit."
        );
    }

    #[test]
    fn questions_convert_and_answers_validate() {
        let d = ask(Some(1), false);
        let c = card(&d);
        assert_eq!(card_of(&d, Language::English, None).unwrap().3, Some(1));
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
        assert_eq!(
            card_of(&ask(Some(1), true), Language::English, None)
                .unwrap()
                .3,
            None
        );
        let b = buttons("s", &c.choices, Some(1), false, " (recommended)");
        assert_eq!(b[0]["components"][1]["style"], 1);
        assert_eq!(b[0]["components"][1]["label"], "Blue (recommended)");
        assert_eq!(b[0]["components"][0]["label"], "Red");
        assert_eq!(b[0]["components"][0]["custom_id"], "cy:s:0");
        // Approvals stay local unless remote approvals are on.
        let approval = dialog_of(&json!({"id": "a", "method": "select", "title": "Run bash?",
            "options": ["Approve Once", "Deny"]}))
        .unwrap();
        assert!(card_of(&approval, Language::English, None).is_none());
        let (kind, body, choices, _) =
            card_of(&approval, Language::Korean, Some(Language::Korean)).unwrap();
        assert_eq!(kind, Kind::Approval);
        assert_eq!(body, "**권한 요청**\nRun bash?");
        assert_eq!(choices, ["이번만 허용", "거부"]);
        let confirm = dialog_of(&json!({"id": "c", "method": "confirm", "title": "Go?"})).unwrap();
        let mut cc = card(&confirm);
        cc.kind = Kind::Confirm;
        assert_eq!(typed_answer(&cc, "2").unwrap(), json!({"confirmed": false}));
        assert!(typed_answer(&cc, "maybe").is_err());
    }

    /// The option OMP recommends carries the owner's language's mark on buttons, menu options and
    /// the card text; a long label is cut, never the mark; nothing is added without a recommended
    /// option.
    #[test]
    fn the_recommended_option_is_marked_in_the_owners_language() {
        let ko = recommended_mark(Language::Korean);
        let en = recommended_mark(Language::English);
        assert_eq!(ko, " (추천)");
        assert_eq!(en, " (recommended)");
        let choices = vec!["Red".to_string(), "가".repeat(120)];
        for mark in [ko, en] {
            let b = buttons("s", &choices, Some(1), false, mark);
            let label = b[0]["components"][1]["label"].as_str().unwrap();
            assert!(label.ends_with(mark), "{label}");
            assert_eq!(label.chars().count(), LABEL_MAX, "{label}");
            assert!(label.starts_with("가가"), "{label}");
            assert_eq!(b[0]["components"][0]["label"], "Red");
            let lines = choice_lines(&choices[..1], Some(0), mark);
            assert_eq!(lines, format!("1. Red{mark}"));
        }
        assert_eq!(marked("Blue", ko, 80), "Blue (추천)");
        // No recommended option: no mark anywhere.
        let plain = buttons("s", &choices, None, false, ko);
        assert!(!plain.to_string().contains("추천"));
        assert!(!choice_lines(&choices, None, ko).contains("추천"));
        // Several questions: the recommended option of a menu carries the mark, cut to fit.
        let q = json!({"id": "a", "question": "a", "options": [{"label": "A"}, {"label": "나".repeat(150)}], "recommended": 1});
        let d =
            dialog_of(&json!({"id": "m", "method": "ask", "questions": [q.clone(), q]})).unwrap();
        let (kind, text, ..) = card_of(&d, Language::Korean, None).unwrap();
        assert!(text.contains(" (추천)"), "{text}");
        let Kind::Asks { questions } = kind else {
            panic!("{kind:?}")
        };
        let items = panel::of_questions(&questions).unwrap();
        let pages = panel::layout("s", &items, ko).unwrap();
        let options = &pages[0][0]["components"][0]["options"];
        assert_eq!(options[0]["label"], "A");
        let long = options[1]["label"].as_str().unwrap();
        assert!(
            long.ends_with(" (추천)") && long.chars().count() == 100,
            "{long}"
        );
    }

    #[test]
    fn a_local_dialog_notice_names_what_waits_and_the_command() {
        let approval = dialog_of(&json!({"id": "a-1", "method": "select",
            "title": "Run bash: rm -rf @everyone *now*", "options": ["Approve Once", "Deny"]}))
        .unwrap();
        let text = local_notice(&approval, false);
        assert!(
            text.starts_with("**Waiting on the machine:** a permission request"),
            "{text}"
        );
        assert!(text.contains("@\u{200b}everyone \\*now\\*"), "{text}");
        assert!(text.contains("(remote approvals are off)"), "{text}");
        assert!(
            text.contains(r#"`cataly lead answer a-1 '{"value":"Approve Once"}'`"#),
            "{text}"
        );
        assert!(text.contains(r#"'{"value":"Deny"}'"#), "{text}");
        assert!(text.encode_utf16().count() <= DISCORD_LIMIT);
        let long =
            dialog_of(&json!({"id": "we`ird", "method": "confirm", "title": "x".repeat(5000)}))
                .unwrap();
        let text = local_notice(&long, true);
        assert!(text.contains("…"), "{text}");
        assert!(text.contains("(it cannot be answered here)"), "{text}");
        assert!(!text.contains("we`ird"), "{text}");
        assert!(text.encode_utf16().count() <= DISCORD_LIMIT);
    }
}
