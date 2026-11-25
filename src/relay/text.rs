//! Text for chat posts, written from DISC-OUT req. 38-45 and OJ req. 55: splitting into parts of at
//! most a given number of UTF-16 units with code-block repair, pipe-table rendering into aligned
//! monospace blocks, and the outgoing text checks.

use std::fmt::Write as _;

use unicode_width::UnicodeWidthStr;

/// Discord's part limit in UTF-16 code units.
pub(crate) const DISCORD_LIMIT: usize = 2000;
/// A rendered table row wider than this leaves the table as it was.
const TABLE_WIDTH_MAX: usize = 80;
/// A message longer than this after rendering is left as it was.
const RENDERED_MAX: usize = 64 * 1024;
/// Room kept in every part for a closing fence line.
const CLOSE: &str = "\n```";

/// Length in UTF-16 code units, the unit Discord counts in.
pub(crate) fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// The longest prefix of `text` within `units` UTF-16 units, as a byte index on a character boundary
/// (so a character that takes two units is never split).
fn byte_index_for(text: &str, units: usize) -> usize {
    let mut used = 0;
    for (i, c) in text.char_indices() {
        used += c.len_utf16();
        if used > units {
            return i;
        }
    }
    text.len()
}

fn backtick_run(text: &str, at: usize) -> usize {
    text.get(at..)
        .map_or(0, |rest| rest.bytes().take_while(|b| *b == b'`').count())
}

/// A fence-language tag is 1-32 letters, digits and `_+-.#`.
fn language(text: &str, after: usize) -> Option<String> {
    let line = text.get(after..)?.split('\n').next()?;
    let ok = (1..=32).contains(&line.len())
        && line
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_+-.#".contains(&b));
    ok.then(|| line.to_string())
}

/// Fence state at the end of `text`: open or not, the open block's language, and where its opener
/// starts. Every run of three or more backticks toggles a block (Discord's pairing).
#[derive(Debug, Default, Clone)]
struct Fence {
    open: bool,
    lang: Option<String>,
    opener: usize,
    runs: usize,
}

fn scan(text: &str) -> Fence {
    let mut fence = Fence::default();
    let mut i = 0;
    while i < text.len() {
        let run = backtick_run(text, i);
        if run >= 3 {
            fence.runs += 1;
            if fence.open {
                fence.open = false;
                fence.lang = None;
            } else {
                fence.open = true;
                fence.opener = i;
                fence.lang = language(text, i + run);
            }
            i += run;
        } else if run > 0 {
            i += run;
        } else {
            i += text
                .get(i..)
                .and_then(|r| r.chars().next())
                .map_or(1, char::len_utf8);
        }
    }
    fence
}

/// If `cut` falls inside an inline code span on its line (outside a block), the start of that span.
fn inline_span_start(text: &str, cut: usize) -> Option<usize> {
    let line_start = text.get(..cut)?.rfind('\n').map_or(0, |i| i + 1);
    let mut open: Option<(usize, usize)> = None;
    let mut i = line_start;
    while i < cut {
        let run = backtick_run(text, i);
        if run == 0 {
            i += text
                .get(i..)
                .and_then(|r| r.chars().next())
                .map_or(1, char::len_utf8);
            continue;
        }
        match open {
            Some((_, n)) if n == run => open = None,
            None if run < 3 => open = Some((i, run)),
            _ => {}
        }
        i += run;
    }
    open.map(|(start, _)| start).filter(|s| *s > 0)
}

/// Where to cut `rest` within `window` bytes: after the last blank line, else the last newline,
/// else the last space in the window's second half; else at the window's end.
fn choose_cut(rest: &str, window: usize) -> usize {
    let Some(head) = rest.get(..window) else {
        return window;
    };
    let mut half = window / 2;
    while !head.is_char_boundary(half) {
        half -= 1;
    }
    let second = head.get(half..).unwrap_or_default();
    let found = second
        .rfind("\n\n")
        .map(|i| i + 2)
        .or_else(|| second.rfind('\n').map(|i| i + 1))
        .or_else(|| second.rfind(' ').map(|i| i + 1));
    found.map_or(window, |i| half + i)
}

/// Splits `text` into parts of at most `limit` UTF-16 units (fence lines included). A part ending
/// inside a code block is closed with a fence and the next part reopens it with the same language.
/// Whitespace-only parts are dropped; `None` when nothing but whitespace remains. Text that fits in
/// one part comes back unchanged.
pub(crate) fn split(text: &str, limit: usize) -> Option<Vec<String>> {
    if text.trim().is_empty() {
        return None;
    }
    if utf16_len(text) <= limit {
        return Some(vec![text.to_string()]);
    }
    let mut parts = Vec::new();
    let mut prefix = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let fixed = utf16_len(&prefix);
        if fixed + utf16_len(rest) <= limit {
            parts.push(format!("{prefix}{rest}"));
            break;
        }
        let budget = limit.saturating_sub(fixed + CLOSE.len()).max(1);
        let window =
            byte_index_for(rest, budget).max(rest.chars().next().map_or(1, char::len_utf8));
        let mut cut = choose_cut(rest, window);
        let mut part = format!("{prefix}{}", rest.get(..cut).unwrap_or_default());
        let mut fence = scan(&part);
        if !fence.open
            && let Some(start) = inline_span_start(&part, part.len()).filter(|s| *s > prefix.len())
        {
            cut = start - prefix.len();
            part.truncate(start);
            fence = scan(&part);
        }
        if fence.open
            && fence.opener > prefix.len()
            && part.get(fence.opener..).is_some_and(|tail| {
                tail.split_once('\n')
                    .is_none_or(|(_, code)| code.trim().is_empty())
            })
        {
            // A block opener with no code after it moves whole to the next part.
            cut = fence.opener - prefix.len();
            part.truncate(fence.opener);
            fence = scan(&part);
        }
        rest = rest.get(cut..).unwrap_or_default();
        let mut part = part.trim_end().to_string();
        prefix.clear();
        if fence.open {
            part.push_str(CLOSE);
            let after = rest.trim_start_matches('\n');
            let run = backtick_run(after, 0);
            if run >= 3 {
                // The block's own closing fence comes right after the cut: consumed, not reopened.
                rest = after.get(run..).unwrap_or_default();
            } else {
                prefix = format!("```{}\n", fence.lang.as_deref().unwrap_or_default());
                rest = rest.trim_start_matches('\n');
            }
        } else {
            rest = rest.trim_start_matches('\n');
        }
        if !part.trim().is_empty() {
            parts.push(part);
        }
    }
    parts.retain(|p| !p.trim().is_empty());
    (!parts.is_empty()).then_some(parts)
}

/// Splits plain text into parts of at most `limit` UTF-16 units whose concatenation is the input
/// exactly (TELE req. 30): each cut comes after the last newline in the back half of the window,
/// else after the last space there, else at the limit, never inside a character.
pub(crate) fn split_plain(text: &str, limit: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = text;
    while utf16_len(rest) > limit {
        let window = byte_index_for(rest, limit).max(rest.chars().next().map_or(1, char::len_utf8));
        let head = rest.get(..window).unwrap_or(rest);
        let mut half = window / 2;
        while !head.is_char_boundary(half) {
            half -= 1;
        }
        let back = head.get(half..).unwrap_or_default();
        let cut = back
            .rfind('\n')
            .or_else(|| back.rfind(' '))
            .map_or(window, |i| half + i + 1);
        let (Some(part), Some(tail)) = (rest.get(..cut), rest.get(cut..)) else {
            break;
        };
        parts.push(part.to_string());
        rest = tail;
    }
    if !rest.is_empty() || parts.is_empty() {
        parts.push(rest.to_string());
    }
    parts
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

/// A line's cells, `Err(())` when the line looks like a row but is ambiguous, `Ok(None)` when it is
/// not a row at all.
fn cells(line: &str) -> Result<Option<Vec<String>>, ()> {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || line.starts_with("    ")
        || line.starts_with('\t')
        || ["#", ">", "- ", "* ", "+ "]
            .iter()
            .any(|m| trimmed.starts_with(m))
        || trimmed
            .split_once(". ")
            .is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    {
        return Ok(None);
    }
    let mut out = Vec::new();
    let mut cell = String::new();
    let mut code: Option<usize> = None;
    let mut pipes = 0;
    let mut chars = trimmed.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c.is_control() {
            return if trimmed.contains('|') {
                Err(())
            } else {
                Ok(None)
            };
        }
        match c {
            '\\' if chars.peek().is_some_and(|(_, n)| *n == '|') && code.is_none() => {
                chars.next();
                cell.push('|');
            }
            '`' => {
                let run = backtick_run(trimmed, i);
                if run >= 3 {
                    return Err(());
                }
                for _ in 1..run {
                    chars.next();
                }
                code = match code {
                    Some(n) if n == run => None,
                    None => Some(run),
                    other => other,
                };
                cell.push_str(&"`".repeat(run));
            }
            '|' if code.is_none() => {
                pipes += 1;
                out.push(std::mem::take(&mut cell));
            }
            c => cell.push(c),
        }
    }
    if code.is_some() {
        return if pipes > 0 { Err(()) } else { Ok(None) };
    }
    if pipes == 0 {
        return Ok(None);
    }
    out.push(cell);
    if trimmed.starts_with('|') {
        out.remove(0);
    }
    if trimmed.ends_with('|') && !trimmed.ends_with("\\|") {
        out.pop();
    }
    Ok(Some(
        out.into_iter().map(|c| c.trim().to_string()).collect(),
    ))
}

fn delimiter(cells: &[String]) -> Option<Vec<Align>> {
    cells
        .iter()
        .map(|c| {
            let inner = c.trim_start_matches(':').trim_end_matches(':');
            if inner.is_empty() || !inner.bytes().all(|b| b == b'-') {
                return None;
            }
            Some(
                match (c.starts_with(':'), c.ends_with(':') && c.len() > 1) {
                    (true, true) => Align::Center,
                    (false, true) => Align::Right,
                    _ => Align::Left,
                },
            )
        })
        .collect()
}

fn pad(cell: &str, width: usize, align: Align) -> String {
    let gap = width.saturating_sub(cell.width());
    let (left, right) = match align {
        Align::Left => (0, gap),
        Align::Right => (gap, 0),
        Align::Center => (gap / 2, gap - gap / 2),
    };
    format!("{}{cell}{}", " ".repeat(left), " ".repeat(right))
}

/// Renders a table as a fenced block, or `None` when a row would be too wide.
fn render(rows: &[Vec<String>], aligns: &[Align]) -> Option<String> {
    let mut widths = vec![1; aligns.len()];
    for row in rows {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(c.width());
        }
    }
    let line = |cells: Vec<String>| cells.join(" | ").trim_end().to_string();
    let mut lines = Vec::with_capacity(rows.len() + 1);
    for (n, row) in rows.iter().enumerate() {
        lines.push(line(
            row.iter()
                .zip(&widths)
                .zip(aligns)
                .map(|((c, w), a)| pad(c, *w, *a))
                .collect(),
        ));
        if n == 0 {
            lines.push(line(widths.iter().map(|w| "-".repeat(*w)).collect()));
        }
    }
    if lines.iter().any(|l| l.width() > TABLE_WIDTH_MAX) {
        return None;
    }
    let mut out = String::from("```\n");
    for l in &lines {
        let _ = writeln!(out, "{l}");
    }
    out.push_str("```");
    Some(out)
}

/// Rewrites GitHub-style pipe tables outside code blocks into aligned monospace blocks. A table with
/// an ambiguous row or a row over 80 columns is left as it is; a message with an unbalanced fence,
/// or one over 64 KiB after rendering, is left whole. Applying it to its own output changes nothing.
pub(crate) fn render_tables(text: &str) -> String {
    if scan(text).runs % 2 == 1 {
        return text.to_string();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_block = false;
    let mut i = 0;
    let mut changed = false;
    while let Some(line) = lines.get(i) {
        let runs = (0..line.len()).filter(|at| {
            backtick_run(line, *at) >= 3 && (*at == 0 || line.as_bytes().get(at - 1) != Some(&b'`'))
        });
        let toggles = runs.count();
        if in_block || toggles > 0 {
            in_block ^= toggles % 2 == 1;
            out.push((*line).to_string());
            i += 1;
            continue;
        }
        if let Some(rendered) = table_at(&lines, i) {
            out.push(rendered.0);
            i += rendered.1;
            changed = true;
            continue;
        }
        out.push((*line).to_string());
        i += 1;
    }
    let result = out.join("\n");
    if !changed || result.len() > RENDERED_MAX {
        return text.to_string();
    }
    result
}

/// A table starting at line `i`: its rendering and how many lines it spans.
fn table_at(lines: &[&str], i: usize) -> Option<(String, usize)> {
    let header = cells(lines.get(i)?).ok()??;
    let aligns = delimiter(&cells(lines.get(i + 1)?).ok()??)?;
    if header.len() != aligns.len() {
        return None;
    }
    let mut rows = vec![header];
    let mut n = i + 2;
    while let Some(line) = lines.get(n) {
        match cells(line) {
            Ok(Some(row)) if row.len() == aligns.len() => rows.push(row),
            Ok(None) => break,
            // A mismatched or ambiguous body row leaves the whole table untouched.
            Ok(Some(_)) | Err(()) => return None,
        }
        n += 1;
    }
    Some((render(&rows, &aligns)?, n - i))
}

/// The owner's outgoing text checks (OJ req. 55).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Checks {
    /// Placeholder words, matched case-insensitively.
    pub(crate) placeholders: Vec<String>,
    /// Characters that must not appear.
    pub(crate) banned: Vec<char>,
}

/// Refuses text with an unfilled template field (`{name}`), a placeholder word or a banned
/// character; the reason names what matched.
pub(crate) fn check(text: &str, checks: &Checks) -> Result<(), String> {
    let bytes = text.as_bytes();
    for (i, _) in text.match_indices('{') {
        let name: String = text
            .get(i + 1..)
            .unwrap_or_default()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let starts_ok = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        if starts_ok && bytes.get(i + 1 + name.len()) == Some(&b'}') {
            return Err(format!("unfilled template field {{{name}}}"));
        }
    }
    let lower = text.to_lowercase();
    if let Some(word) = checks
        .placeholders
        .iter()
        .find(|w| !w.is_empty() && lower.contains(&w.to_lowercase()))
    {
        return Err(format!("placeholder word '{word}'"));
    }
    if let Some(c) = text.chars().find(|c| checks.banned.contains(c)) {
        return Err(format!("banned character U+{:04X}", u32::from(c)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(parts: &[String]) -> String {
        parts.concat()
    }

    #[test]
    fn short_text_is_identical_and_blank_is_refused() {
        assert_eq!(split("  hi\n", 2000).unwrap(), ["  hi\n"]);
        assert!(split(" \n\t", 2000).is_none());
        assert_eq!(utf16_len("a😀"), 3);
    }

    #[test]
    fn cuts_prefer_blank_lines_then_newlines_then_spaces() {
        let text = format!("{}\n\n{}", "a".repeat(30), "b".repeat(30));
        let parts = split(&text, 40).unwrap();
        assert_eq!(parts, ["a".repeat(30), "b".repeat(30)]);
        let words = "word ".repeat(30);
        for p in split(&words, 40).unwrap() {
            assert!(utf16_len(&p) <= 40);
            assert!(!p.starts_with(' '));
        }
        let solid = "x".repeat(95);
        let parts = split(&solid, 40).unwrap();
        assert_eq!(joined(&parts), solid);
        assert!(parts.iter().all(|p| utf16_len(p) <= 40));
    }

    #[test]
    fn surrogate_pairs_stay_together() {
        let text = "😀".repeat(30);
        let parts = split(&text, 11).unwrap();
        assert!(parts.iter().all(|p| utf16_len(p) <= 11));
        assert_eq!(joined(&parts), text);
    }

    #[test]
    fn code_blocks_are_closed_and_reopened_with_their_language() {
        let code: String = (0..40).fold(String::new(), |mut s, i| {
            let _ = writeln!(s, "let x{i} = {i};");
            s
        });
        let text = format!("intro\n```rust\n{code}```\nafter");
        let parts = split(&text, 200).unwrap();
        assert!(parts.len() > 2);
        for p in &parts {
            assert!(utf16_len(p) <= 200, "{p}");
            assert_eq!(scan(p).runs % 2, 0, "every part has balanced fences: {p}");
        }
        assert!(parts[1].starts_with("```rust\n") || parts[0].ends_with("```"));
        assert!(
            parts
                .iter()
                .skip(1)
                .filter(|p| p.contains("let"))
                .all(|p| p.starts_with("```rust\n"))
        );
        assert!(parts.last().unwrap().ends_with("after"));
    }

    #[test]
    fn an_opener_without_code_moves_to_the_next_part() {
        let text = format!("{}\n```sh\n{}\n```", "w ".repeat(14), "y".repeat(20));
        let parts = split(&text, 40).unwrap();
        assert!(!parts[0].contains("```"), "{parts:?}");
        assert!(parts[1].starts_with("```sh\ny"), "{parts:?}");
    }

    #[test]
    fn inline_code_span_is_not_split() {
        let text = format!("{} `code span here` tail", "a".repeat(25));
        let parts = split(&text, 40).unwrap();
        assert!(
            parts.iter().all(|p| p.matches('`').count() % 2 == 0),
            "{parts:?}"
        );
    }

    #[test]
    fn many_random_texts_keep_the_limit_and_their_content() {
        let mut seed: u64 = 7;
        let pieces = [
            "word ",
            "\n",
            "\n\n",
            "```py\n",
            "`x` ",
            "😀",
            "é",
            "| a | b |\n",
        ];
        for _ in 0..300 {
            let mut text = String::new();
            for _ in 0..200 {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                text.push_str(pieces[(seed >> 33) as usize % pieces.len()]);
            }
            let Some(parts) = split(&text, 120) else {
                continue;
            };
            for p in &parts {
                assert!(utf16_len(p) <= 120, "{p:?}");
                assert!(!p.trim().is_empty(), "no blank part");
            }
            let letters = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).count();
            let (before, after) = (letters(&text), letters(&joined(&parts)));
            // Only fence-language tags are added or consumed.
            assert!(before.abs_diff(after) <= 2 * 40 * parts.len());
        }
    }

    #[test]
    fn tables_render_aligned_and_idempotent() {
        let text = "Result:\n| name | n |\n|:--|--:|\n| alpha | 1 |\n| b | 22 |\ndone";
        let out = render_tables(text);
        assert_eq!(
            out,
            "Result:\n```\nname  |  n\n----- | --\nalpha |  1\nb     | 22\n```\ndone"
        );
        assert_eq!(render_tables(&out), out);
        let wide = "| 名前 | x |\n|---|---|\n| 漢字漢字 | y |";
        assert!(render_tables(wide).contains("漢字漢字 | y"));
        assert!(render_tables(wide).contains("名前     | x"));
    }

    #[test]
    fn ambiguous_or_wide_tables_and_code_are_untouched() {
        for text in [
            "| a | b |\n|---|---|\n| 1 | 2 | 3 |",
            "| a | `b |\n|---|---|\n| 1 | 2 |",
            &format!("| {} | b |\n|---|---|\n| 1 | 2 |", "x".repeat(90)),
            "```\n| a | b |\n|---|---|\n```",
            "```\nopen fence\n| a | b |\n|---|---|",
            "- | a | b |\n|---|---|",
            "| a \\| b | c |",
        ] {
            assert_eq!(render_tables(text), text, "{text}");
        }
        let escaped = render_tables("| a \\| b | c |\n|---|---|\n| `x|y` | 2 |");
        assert!(escaped.contains("a | b | c"), "{escaped}");
        assert!(escaped.contains("`x|y` | 2"), "{escaped}");
    }

    #[test]
    fn outgoing_checks() {
        let checks = Checks {
            placeholders: vec!["lorem ipsum".into()],
            banned: vec!['\u{2014}'],
        };
        assert!(check("all good {\"a\": 1} { spaced }", &checks).is_ok());
        assert_eq!(
            check("hello {name}", &checks).unwrap_err(),
            "unfilled template field {name}"
        );
        assert!(
            check("Lorem Ipsum here", &checks)
                .unwrap_err()
                .contains("placeholder")
        );
        assert_eq!(
            check("a \u{2014} b", &checks).unwrap_err(),
            "banned character U+2014"
        );
    }
}
