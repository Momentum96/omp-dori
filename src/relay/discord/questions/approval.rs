//! The remote approval card (DESIGN 12.2 A18) in the owner's language (A19): a title, the tool and
//! the reason as labelled lines, and the command in a code block, cut at a line or word boundary
//! when it is long. OMP's prompt text (`Allow tool: …`, `Reason: …`, `Command: …`, other detail lines)
//! is read line by line; a line in another shape is shown as it is.

use std::fmt::Write as _;

use super::neutral;
use crate::config::Language;
use crate::host::Dialog;

/// The longest command shown, in characters, and its most lines.
const COMMAND_MAX: usize = 900;
const COMMAND_LINES: usize = 20;
/// The longest reason and the longest other detail text shown, in characters.
const DETAIL_MAX: usize = 300;

/// The card's fixed words in one language.
pub(super) struct Words {
    head: &'static str,
    tool: &'static str,
    reason: &'static str,
    command: &'static str,
    /// The note under a cut command; `{}` is the count of characters shown.
    cut: &'static str,
    /// The two buttons: approve this call once, deny it.
    pub(super) buttons: [&'static str; 2],
    /// The line added when the owner pressed approve or deny.
    pub(super) answered: [&'static str; 2],
    /// The line added when the run ended before an answer.
    pub(super) run_ended: &'static str,
}

const KOREAN: Words = Words {
    head: "권한 요청",
    tool: "도구",
    reason: "사유",
    command: "명령",
    cut: "(명령이 길어 앞부분 {}자만 보여 줍니다)",
    buttons: ["이번만 허용", "거부"],
    answered: ["보스가 허용했습니다", "보스가 거부했습니다"],
    run_ended: "답을 받기 전에 실행이 끝나 거부되었습니다.",
};

const ENGLISH: Words = Words {
    head: "Permission asked",
    tool: "Tool",
    reason: "Reason",
    command: "Command",
    cut: "(long command: the first {} characters are shown)",
    buttons: ["Approve once", "Deny"],
    answered: [
        "Answered by the owner: Approve once",
        "Answered by the owner: Deny",
    ],
    run_ended: "Denied: the run ended before an answer.",
};

pub(super) const fn words(language: Language) -> &'static Words {
    match language {
        Language::Korean => &KOREAN,
        Language::English => &ENGLISH,
    }
}

/// `text` cut to `max` characters with `…`; whole when it fits.
fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// The command cut to [`COMMAND_LINES`] lines and [`COMMAND_MAX`] characters, ending at a line break
/// or a space when one is near the cut; the second value says whether it was cut.
fn cut_command(command: &str) -> (String, bool) {
    let lines: Vec<&str> = command.lines().collect();
    let kept = lines
        .get(..COMMAND_LINES)
        .map_or_else(|| command.to_string(), |l| l.join("\n"));
    let mut cut = kept.len() < command.trim_end().len();
    if kept.chars().count() <= COMMAND_MAX {
        return (kept, cut);
    }
    cut = true;
    let head: String = kept.chars().take(COMMAND_MAX).collect();
    let boundary = head.rfind(['\n', ' ']).filter(|at| {
        head.get(..*at)
            .is_some_and(|h| h.chars().count() >= COMMAND_MAX * 3 / 4)
    });
    let head = boundary
        .and_then(|at| head.get(..at))
        .map_or(head.clone(), str::to_string);
    (head.trim_end().to_string(), cut)
}

/// Text for a code block: no run of backquotes can close it, and no mention can form.
fn fenced(text: &str) -> String {
    let body = text.replace('`', "`\u{200b}").replace('@', "@\u{200b}");
    format!("```\n{body}\n```")
}

/// The card for an approval dialog: its text and the two button labels.
pub(super) fn card(dialog: &Dialog, language: Language) -> (String, Vec<String>) {
    let prompt: Vec<&str> = [&dialog.title, &dialog.message]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let buttons = words(language).buttons.map(String::from).to_vec();
    (text(&prompt.join("\n"), language), buttons)
}

/// The card's text for OMP's approval prompt `prompt` (its title and message, line by line).
fn text(prompt: &str, language: Language) -> String {
    let w = words(language);
    let mut out = format!("**{}**", w.head);
    let mut others = String::new();
    let mut lines = prompt.lines();
    let mut command: Option<String> = None;
    while let Some(line) = lines.next() {
        if let Some(tool) = line.strip_prefix("Allow tool: ") {
            let tool: String = tool.chars().filter(|c| *c != '`').collect();
            let _ = write!(out, "\n**{}:** `{}`", w.tool, short(&tool, 80));
        } else if let Some(reason) = line.strip_prefix("Reason: ") {
            let reason = neutral(&short(reason, DETAIL_MAX));
            let _ = write!(out, "\n**{}:** {reason}", w.reason);
        } else if let Some(first) = line.strip_prefix("Command:") {
            // The command runs to the end of the prompt; OMP puts it last.
            let mut all = first.trim_start().to_string();
            for rest in lines.by_ref() {
                all.push('\n');
                all.push_str(rest);
            }
            command = Some(all.trim_matches('\n').to_string());
        } else if !line.trim().is_empty() {
            others.push('\n');
            others.push_str(line);
        }
    }
    if !others.is_empty() {
        out.push_str(&neutral(&short(&others, DETAIL_MAX)));
    }
    if let Some(command) = command {
        let (shown, was_cut) = cut_command(&command);
        let _ = write!(out, "\n**{}:**\n{}", w.command, fenced(&shown));
        if was_cut {
            out.push('\n');
            out.push_str(&w.cut.replace("{}", &shown.chars().count().to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER_LINE: &str = "Allow tool: bash\nCommand: cataly_lead request.close --id 1 --text \"done\" 2>&1; cataly_lead requests";

    #[test]
    fn the_card_speaks_korean_and_shows_the_command_in_a_code_block() {
        let card = text(OWNER_LINE, Language::Korean);
        assert_eq!(
            card,
            "**권한 요청**\n**도구:** `bash`\n**명령:**\n```\ncataly_lead request.close --id 1 --text \"done\" 2>&1; cataly_lead requests\n```"
        );
        let w = words(Language::Korean);
        assert_eq!(w.buttons, ["이번만 허용", "거부"]);
        assert_eq!(w.answered[0], "보스가 허용했습니다");
        let english = text(OWNER_LINE, Language::English);
        assert!(english.starts_with("**Permission asked**\n**Tool:** `bash`\n**Command:**\n```\n"));
        assert_eq!(words(Language::English).buttons, ["Approve once", "Deny"]);
    }

    #[test]
    fn reasons_and_other_lines_stay_neutral_and_a_command_cannot_close_its_block() {
        let prompt = "Allow tool: bash\nReason: Critical pattern detected\nCommand: echo ```@everyone``` && rm -rf x";
        let card = text(prompt, Language::Korean);
        assert!(
            card.contains("**사유:** Critical pattern detected"),
            "{card}"
        );
        assert_eq!(card.matches("```").count(), 2, "{card}");
        assert!(!card.contains("@everyone"), "{card}");
        let odd = text("Run bash: rm -rf @everyone *now*", Language::English);
        assert!(odd.contains("@\u{200b}everyone \\*now\\*"), "{odd}");
    }

    #[test]
    fn a_long_command_is_cut_at_a_word_or_line_and_says_so() {
        let long = format!("Allow tool: bash\nCommand: {}", "word ".repeat(400));
        let card = text(&long, Language::Korean);
        assert!(card.chars().count() < 1200, "{}", card.chars().count());
        let block = card.split("```").nth(1).unwrap_or_default();
        assert!(block.trim_end().ends_with("word"), "{block}");
        assert!(card.ends_with("자만 보여 줍니다)"), "{card}");
        let many = format!("Allow tool: bash\nCommand: {}", "ls\n".repeat(50));
        let card = text(&many, Language::English);
        assert_eq!(
            card.split("```")
                .nth(1)
                .unwrap_or_default()
                .matches("ls")
                .count(),
            20
        );
        assert!(card.contains("the first"), "{card}");
        let short_card = text("Allow tool: bash\nCommand: ls", Language::English);
        assert!(!short_card.contains("first"), "{short_card}");
    }
}
