//! Native Discord answers for forms with several fields and `ask` dialogs with several questions
//! (DISC-OUT req. 63, 72): a select menu per choice field, a modal with text inputs (five at most
//! per modal) for text fields, and a Submit button. When one message cannot hold them all (five
//! component rows), each field gets its own message and the last one carries Submit. The owner
//! never types JSON; the hub checks number ranges, text lengths and required fields on submit.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::super::form::{Field, FieldKind, Limits};
use super::super::types::Press;
use super::{Card, Kind, State, neutral};
use crate::host::Runtime;
use crate::host::dialogs::Question;
use crate::relay::discord::intake::Intake;

/// A select holds at most 25 options; a message at most 5 rows; a modal at most 5 inputs.
const OPTIONS_MAX: usize = 25;
const ROWS_MAX: usize = 5;
const INPUTS_MAX: usize = 5;
const LABEL_MAX: usize = 45;
const TEXT_MAX: usize = 4000;

/// One thing the owner answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Item {
    Choice {
        label: String,
        options: Vec<String>,
        multi: bool,
        required: bool,
        default: Vec<usize>,
    },
    Text {
        label: String,
        required: bool,
        limits: Limits,
        default: String,
    },
}

/// The owner's answer to one item so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Val {
    Unset,
    Picks(Vec<usize>),
    Text(String),
}

fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The items of a form's editable fields; `None` when a choice has more than 25 options.
pub(super) fn of_fields(fields: &[Field]) -> Option<Vec<Item>> {
    fields
        .iter()
        .map(|f| {
            let label = if f.label.is_empty() {
                f.name.clone()
            } else {
                f.label.clone()
            };
            match f.kind {
                FieldKind::One | FieldKind::Many => {
                    (f.choices.len() <= OPTIONS_MAX).then(|| Item::Choice {
                        label,
                        options: f.choices.iter().map(|c| c.label.clone()).collect(),
                        multi: f.kind == FieldKind::Many,
                        required: f.required,
                        default: f
                            .choices
                            .iter()
                            .enumerate()
                            .filter(|(_, c)| c.selected)
                            .map(|(i, _)| i)
                            .collect(),
                    })
                }
                FieldKind::Text | FieldKind::Hidden => Some(Item::Text {
                    label,
                    required: f.required,
                    limits: f.limits.clone(),
                    default: f.value.clone(),
                }),
            }
        })
        .collect()
}

/// The items of an `ask` dialog: a choice per question with options, text otherwise. A question
/// without a recommended choice must be answered.
pub(super) fn of_questions(questions: &[Question]) -> Option<Vec<Item>> {
    questions
        .iter()
        .map(|q| {
            if q.options.is_empty() {
                return Some(Item::Text {
                    label: q.text.clone(),
                    required: true,
                    limits: Limits::default(),
                    default: String::new(),
                });
            }
            (q.options.len() <= OPTIONS_MAX).then(|| Item::Choice {
                label: q.text.clone(),
                options: q.options.clone(),
                multi: q.multi,
                required: q.recommended.is_none(),
                default: q.recommended.into_iter().collect(),
            })
        })
        .collect()
}

pub(super) fn draft(items: &[Item]) -> Vec<Val> {
    items
        .iter()
        .map(|i| match i {
            Item::Choice { default, .. } if !default.is_empty() => Val::Picks(default.clone()),
            Item::Text { default, .. } if !default.is_empty() => Val::Text(default.clone()),
            _ => Val::Unset,
        })
        .collect()
}

fn label(i: &Item) -> &str {
    match i {
        Item::Choice { label, .. } | Item::Text { label, .. } => label,
    }
}

fn select(stamp: &str, n: usize, item: &Item) -> Value {
    let Item::Choice {
        label,
        options,
        multi,
        default,
        ..
    } = item
    else {
        return Value::Null;
    };
    let opts: Vec<Value> = options
        .iter()
        .enumerate()
        .map(|(i, o)| json!({ "label": cut(o, 100), "value": i.to_string(), "default": default.contains(&i) }))
        .collect();
    json!({ "type": 1, "components": [{
        "type": 3, "custom_id": format!("cy:{stamp}:s:{n}"), "placeholder": cut(label, 150),
        "min_values": i32::from(!*multi), "max_values": if *multi { options.len() } else { 1 },
        "options": opts }] })
}

fn button(custom_id: &str, label: &str, style: u8) -> Value {
    json!({ "type": 2, "style": style, "label": cut(label, 80), "custom_id": custom_id })
}

fn list(ids: &[usize]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("-")
}

/// The components of each message: one message when it fits, else one per item (the last also
/// carries Submit). `None` when even one item per message cannot be shown.
pub(super) fn layout(stamp: &str, items: &[Item]) -> Option<Vec<Value>> {
    let choices: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, i)| matches!(i, Item::Choice { .. }))
        .map(|(n, _)| n)
        .collect();
    let texts: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, i)| matches!(i, Item::Text { .. }))
        .map(|(n, _)| n)
        .collect();
    let groups: Vec<&[usize]> = texts.chunks(INPUTS_MAX).collect();
    let submit = button(&format!("cy:{stamp}:go"), "Submit", 3);
    if choices.len() < ROWS_MAX && groups.len() < ROWS_MAX {
        let mut rows: Vec<Value> = choices
            .iter()
            .filter_map(|n| Some(select(stamp, *n, items.get(*n)?)))
            .collect();
        let mut buttons: Vec<Value> = groups
            .iter()
            .enumerate()
            .map(|(g, ids)| {
                let name = if groups.len() == 1 {
                    "Fill in".to_string()
                } else {
                    format!("Fill in, part {}", g + 1)
                };
                button(&format!("cy:{stamp}:m:{}", list(ids)), &name, 1)
            })
            .collect();
        buttons.push(submit);
        rows.push(json!({ "type": 1, "components": buttons }));
        return Some(vec![Value::Array(rows)]);
    }
    let last = items.len().checked_sub(1)?;
    let messages = items
        .iter()
        .enumerate()
        .map(|(n, item)| {
            let mut rows = match item {
                Item::Choice { .. } => vec![select(stamp, n, item)],
                Item::Text { .. } => {
                    vec![json!({ "type": 1, "components": [button(&format!("cy:{stamp}:m:{n}"), "Answer", 1)] })]
                }
            };
            if n == last {
                rows.push(json!({ "type": 1, "components": [submit.clone()] }));
            }
            Value::Array(rows)
        })
        .collect();
    Some(messages)
}

/// The modal for the text items in `ids`, filled with the answers so far.
fn modal(stamp: &str, items: &[Item], ids: &[usize], draft: &[Val]) -> Value {
    let rows: Vec<Value> = ids
        .iter()
        .filter_map(|n| {
            let Some(Item::Text {
                label,
                required,
                limits,
                ..
            }) = items.get(*n)
            else {
                return None;
            };
            let mut input = Map::new();
            input.insert("type".into(), json!(4));
            input.insert("custom_id".into(), json!(format!("i{n}")));
            input.insert("label".into(), json!(cut(label, LABEL_MAX)));
            input.insert("style".into(), json!(if limits.long { 2 } else { 1 }));
            input.insert("required".into(), json!(*required));
            input.insert(
                "max_length".into(),
                json!(limits.max_len.unwrap_or(TEXT_MAX).min(TEXT_MAX)),
            );
            if let Some(min) = limits.min_len {
                input.insert("min_length".into(), json!(min.min(TEXT_MAX)));
            }
            if limits.number {
                let range = format!(
                    "a number from {} to {}",
                    limits.min.as_deref().unwrap_or("any"),
                    limits.max.as_deref().unwrap_or("any")
                );
                input.insert("placeholder".into(), json!(cut(&range, 100)));
            }
            if let Some(Val::Text(t)) = draft.get(*n) {
                input.insert("value".into(), json!(cut(t, TEXT_MAX)));
            }
            Some(json!({ "type": 1, "components": [input] }))
        })
        .collect();
    json!({ "type": 9, "data": { "custom_id": format!("cy:{stamp}:f:{}", list(ids)), "title": "Answer for the lead", "components": rows } })
}

/// Checks each item and the required ones (DISC-OUT req. 72); every problem is named.
fn problems(items: &[Item], draft: &[Val]) -> Vec<String> {
    let mut out = Vec::new();
    for (item, val) in items.iter().zip(draft) {
        let name = neutral(label(item));
        match (item, val) {
            (
                Item::Choice { required: true, .. } | Item::Text { required: true, .. },
                Val::Unset,
            ) => {
                out.push(format!("{name} is required"));
            }
            (Item::Choice { required: true, .. }, Val::Picks(p)) if p.is_empty() => {
                out.push(format!("{name} is required"));
            }
            (
                Item::Text {
                    required, limits, ..
                },
                Val::Text(t),
            ) => {
                if *required && t.trim().is_empty() {
                    out.push(format!("{name} is required"));
                } else if let Err(e) = limits.check(&name, t) {
                    out.push(e);
                }
            }
            _ => {}
        }
    }
    out
}

/// The answer OMP takes for a draft, or the problems to show the owner.
pub(super) fn answer(kind: &Kind, items: &[Item], draft: &[Val]) -> Result<Value, Vec<String>> {
    let problems = problems(items, draft);
    if !problems.is_empty() {
        return Err(problems);
    }
    match kind {
        Kind::Fields { fields, hidden } => {
            let mut data: Map<String, Value> = hidden.clone();
            for ((f, item), val) in fields.iter().zip(items).zip(draft) {
                let value = match (item, val) {
                    (Item::Choice { multi, .. }, Val::Picks(p)) => {
                        let picked: Vec<Value> = p
                            .iter()
                            .filter_map(|i| f.choices.get(*i))
                            .map(|c| json!(c.value))
                            .collect();
                        if *multi {
                            Value::Array(picked)
                        } else {
                            picked.into_iter().next().unwrap_or(Value::Null)
                        }
                    }
                    (Item::Choice { multi: true, .. }, _) => json!([]),
                    (Item::Choice { .. }, _) => Value::Null,
                    (Item::Text { .. }, Val::Text(t)) => json!(t),
                    (Item::Text { .. }, _) => json!(""),
                };
                data.insert(f.name.clone(), value);
            }
            Ok(json!({ "data": data }))
        }
        Kind::Asks { questions } => {
            let answers: Vec<Value> = questions
                .iter()
                .zip(draft)
                .map(|(q, val)| match val {
                    Val::Picks(p) => {
                        let labels: Vec<&String> =
                            p.iter().filter_map(|i| q.options.get(*i)).collect();
                        json!({ "id": q.id, "selectedOptions": labels })
                    }
                    Val::Text(t) => json!({ "id": q.id, "selectedOptions": [], "customInput": t }),
                    Val::Unset => json!({ "id": q.id, "selectedOptions": [] }),
                })
                .collect();
            Ok(json!({ "answers": answers }))
        }
        _ => Err(vec!["this question has no fields".into()]),
    }
}

fn ids(text: &str) -> Vec<usize> {
    text.split('-').filter_map(|n| n.parse().ok()).collect()
}

impl<T: Runtime> Intake<T> {
    async fn reply(&self, press: &Press, body: Value) -> bool {
        match self.rest.callback(&press.id, &press.token, body).await {
            Ok(()) => true,
            Err(e) => {
                crate::log::info("interaction not answered", &[("error", &e)]);
                false
            }
        }
    }

    async fn tell(&self, press: &Press, text: &str) {
        self.reply(
            press,
            json!({"type": 4, "data": {"content": text, "flags": 64}}),
        )
        .await;
    }

    /// A select, a modal button, a modal submit or Submit on a card with fields. The owner check
    /// and the card lookup are done by the caller.
    pub(super) async fn panel_press(self: Arc<Self>, card: Card, action: &str, press: Press) {
        let Some(items) = card.items.clone() else {
            return self
                .tell(&press, "This button is not part of the question.")
                .await;
        };
        if let Some(n) = action
            .strip_prefix("s:")
            .and_then(|n| n.parse::<usize>().ok())
        {
            let picks: Option<Vec<usize>> = press.values.iter().map(|v| v.parse().ok()).collect();
            let fits = matches!(items.get(n), Some(Item::Choice { options, .. }) if picks.as_ref().is_some_and(|p| p.iter().all(|i| *i < options.len())));
            if !fits || !self.reply(&press, json!({"type": 6})).await {
                return;
            }
            return self
                .cards
                .set(&card.dialog, n, Val::Picks(picks.unwrap_or_default()));
        }
        if let Some(list) = action.strip_prefix("m:") {
            let wanted = ids(list);
            if wanted.is_empty()
                || wanted
                    .iter()
                    .any(|n| !matches!(items.get(*n), Some(Item::Text { .. })))
            {
                return self
                    .tell(&press, "This button is not part of the question.")
                    .await;
            }
            let draft = self.cards.draft(&card.dialog);
            self.reply(&press, modal(&card.stamp, &items, &wanted, &draft))
                .await;
            return;
        }
        if let Some(list) = action.strip_prefix("f:") {
            let wanted = ids(list);
            let mut errors = Vec::new();
            for n in &wanted {
                let Some(Item::Text { label, limits, .. }) = items.get(*n) else {
                    continue;
                };
                let given = press
                    .inputs
                    .iter()
                    .find(|(id, _)| *id == format!("i{n}"))
                    .map_or_default(|(_, v)| v.clone());
                match limits.check(&neutral(label), &given) {
                    Ok(()) => self.cards.set(
                        &card.dialog,
                        *n,
                        if given.is_empty() {
                            Val::Unset
                        } else {
                            Val::Text(given)
                        },
                    ),
                    Err(e) => errors.push(e),
                }
            }
            if errors.is_empty() {
                self.reply(&press, json!({"type": 6})).await;
            } else {
                self.tell(
                    &press,
                    &format!("Not taken: {}. Use Fill in again.", errors.join("; ")),
                )
                .await;
            }
            return;
        }
        if action != "go" {
            return self
                .tell(&press, "This button is not part of the question.")
                .await;
        }
        let Some(card) = self.cards.take(&card.dialog) else {
            return;
        };
        match answer(&card.kind, &items, &card.draft) {
            Err(problems) => {
                self.cards.reopen(&card.dialog);
                self.tell(&press, &format!("Not submitted: {}.", problems.join("; ")))
                    .await;
            }
            Ok(value) => {
                if !self.reply(&press, json!({"type": 6})).await {
                    return self.cards.reopen(&card.dialog);
                }
                let done = format!("{}\n\nAnswered by the owner.", card.body);
                self.submit_value(card, value, done).await;
            }
        }
    }
}

impl super::Cards {
    fn set(&self, dialog: &str, n: usize, val: Val) {
        let mut inner = self.lock();
        if let Some(slot) = inner
            .open
            .get_mut(dialog)
            .filter(|c| c.state == State::Open)
            .and_then(|c| c.draft.get_mut(n))
        {
            *slot = val;
        }
    }

    fn draft(&self, dialog: &str) -> Vec<Val> {
        self.lock()
            .open
            .get(dialog)
            .map_or_default(|c| c.draft.clone())
    }
}
