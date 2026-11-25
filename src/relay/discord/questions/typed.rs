//! Typed answers to question cards (DISC-OUT req. 70-73): the texts around them and the field-limit
//! messages (number ranges, text lengths, required fields) in the owner's language
//! (`[hub] language`), and a typed answer to one part of a card with several questions or fields.

use super::super::form::{Limits, Misfit};
use super::panel::{Item, Val};
use super::{Card, Kind, neutral, pick};
use crate::config::Language;

/// The texts around typed answers. A pair is the text before and after the inserted value.
pub(super) struct Words {
    /// Under a question that takes a typed answer.
    pub(super) hint: &'static str,
    /// Under a question with menus.
    pub(super) panel_hint: &'static str,
    /// Added to a card answered by a typed reply.
    pub(super) answered: &'static str,
    /// A reply to a card with several open parts that names none of them.
    pub(super) which: &'static str,
    /// An answer that is empty or too long.
    pub(super) length: &'static str,
    retry: [&'static str; 2],
    number: [&'static str; 2],
    recorded: [&'static str; 2],
    /// A required part left without an answer, around its name.
    required: [&'static str; 2],
    /// Fill in answers that were refused, around the problems.
    not_taken: [&'static str; 2],
    /// Submit refused, around the problems.
    not_submitted: [&'static str; 2],
    language: Language,
}

pub(super) const fn words(language: Language) -> Words {
    match language {
        Language::Korean => Words {
            hint: "번호를 누르거나, 원하는 답을 채팅으로 보내 주세요.",
            panel_hint: "메뉴에서 고르고 Fill in으로 글을 채운 뒤 Submit을 눌러 주세요. 질문 하나에 답장으로 답할 수도 있어요.",
            answered: "보스가 채팅으로 답했습니다.",
            which: "열린 질문이 여러 개예요. 답할 질문의 메시지에 답장하거나 메뉴를 써 주세요",
            length: "답은 1자에서 2000자까지예요",
            retry: ["그 답은 받을 수 없어요: ", ". 다시 보내 주세요."],
            number: ["1부터 ", "까지의 번호로 답해 주세요"],
            recorded: [
                "",
                "에 대한 답을 받았어요. 나머지를 채운 뒤 Submit을 눌러 주세요.",
            ],
            required: ["", ": 꼭 답해 주세요"],
            not_taken: ["받지 못했어요: ", ". Fill in을 다시 눌러 주세요."],
            not_submitted: ["제출하지 못했어요: ", "."],
            language,
        },
        Language::English => Words {
            hint: "Reply to this message with a number or your answer.",
            panel_hint: "Choose in the menus, use Fill in for text, then press Submit. You can also reply to one question.",
            answered: "Answered by the owner's reply.",
            which: "several parts are open; reply to that part's message or use the menus",
            length: "answers are 1 to 2000 characters",
            retry: ["That answer was not taken: ", ". Please try again."],
            number: ["answer with a number from 1 to ", ""],
            recorded: ["Taken for ", "; fill in the rest, then press Submit."],
            required: ["", " is required"],
            not_taken: ["Not taken: ", ". Use Fill in again."],
            not_submitted: ["Not submitted: ", "."],
            language,
        },
    }
}

impl Words {
    pub(super) fn retry(&self, problem: &str) -> String {
        format!("{}{problem}{}", self.retry[0], self.retry[1])
    }

    pub(super) fn number(&self, last: usize) -> String {
        format!("{}{last}{}", self.number[0], self.number[1])
    }

    pub(super) fn recorded(&self, part: &str) -> String {
        format!("{}{}{}", self.recorded[0], neutral(part), self.recorded[1])
    }

    pub(super) fn required(&self, name: &str) -> String {
        format!("{}{name}{}", self.required[0], self.required[1])
    }

    pub(super) fn not_taken(&self, problems: &[String]) -> String {
        let [before, after] = self.not_taken;
        format!("{before}{}{after}", problems.join("; "))
    }

    pub(super) fn not_submitted(&self, problems: &[String]) -> String {
        let [before, after] = self.not_submitted;
        format!("{before}{}{after}", problems.join("; "))
    }

    /// Checks `value` against a field's limits; a misfit names the field `name`.
    pub(super) fn check(&self, name: &str, limits: &Limits, value: &str) -> Result<(), String> {
        limits
            .check(value)
            .map_err(|misfit| self.misfit(name, limits, misfit))
    }

    fn misfit(&self, name: &str, limits: &Limits, misfit: Misfit) -> String {
        let least = limits.min_len.unwrap_or(0);
        match (self.language, misfit) {
            (Language::English, Misfit::Length) => format!(
                "{name} takes {least} to {} characters",
                limits
                    .max_len
                    .map_or_else(|| "any number of".into(), |m| m.to_string())
            ),
            (Language::Korean, Misfit::Length) => match limits.max_len {
                Some(most) => format!("{name}: {least}자에서 {most}자까지 써 주세요"),
                None => format!("{name}: {least}자 이상 써 주세요"),
            },
            (Language::English, Misfit::NotNumber) => format!("{name} takes a number"),
            (Language::Korean, Misfit::NotNumber) => format!("{name}: 숫자로 답해 주세요"),
            (Language::English, Misfit::Range) => format!("{name} takes {}", self.range(limits)),
            (Language::Korean, Misfit::Range) => {
                format!("{name}: {}로 답해 주세요", self.range(limits))
            }
        }
    }

    /// The numbers a field takes, as its placeholder and in its misfit.
    pub(super) fn range(&self, limits: &Limits) -> String {
        let (min, max) = (limits.min.as_deref(), limits.max.as_deref());
        match self.language {
            Language::English => format!(
                "a number from {} to {}",
                min.unwrap_or("any"),
                max.unwrap_or("any")
            ),
            Language::Korean => match (min, max) {
                (Some(low), Some(high)) => format!("{low}부터 {high}까지의 숫자"),
                (Some(low), None) => format!("{low} 이상의 숫자"),
                (None, Some(high)) => format!("{high} 이하의 숫자"),
                (None, None) => "숫자".into(),
            },
        }
    }
}

/// The part a typed answer is for: the part whose message was replied to, else the only part
/// still without an answer.
pub(super) fn part_of(card: &Card, replied: Option<usize>) -> Option<usize> {
    if replied.is_some() {
        return replied;
    }
    let mut open = card
        .draft
        .iter()
        .enumerate()
        .filter(|(_, v)| **v == Val::Unset)
        .map(|(n, _)| n);
    match (open.next(), open.next()) {
        (Some(n), None) => Some(n),
        _ => None,
    }
}

/// A typed answer to part `n`: option numbers or labels for a choice (comma-separated where
/// several may be picked), free text where OMP takes it (an `ask` question, a text field).
pub(super) fn part_value(card: &Card, n: usize, text: &str, words: &Words) -> Result<Val, String> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > 2000 {
        return Err(words.length.into());
    }
    match card.items.as_ref().and_then(|items| items.get(n)) {
        Some(Item::Choice { options, multi, .. }) => {
            let given: Vec<&str> = if *multi {
                text.split(',').collect()
            } else {
                vec![text]
            };
            let picked: Option<Vec<usize>> = given.iter().map(|g| pick(options, g)).collect();
            match picked {
                Some(p) => Ok(Val::Picks(p)),
                None if matches!(card.kind, Kind::Asks { .. }) => Ok(Val::Text(text.to_string())),
                None => Err(words.number(options.len())),
            }
        }
        Some(Item::Text { label, limits, .. }) => {
            words.check(&neutral(label), limits, text)?;
            Ok(Val::Text(text.to_string()))
        }
        None => Err(words.which.into()),
    }
}
