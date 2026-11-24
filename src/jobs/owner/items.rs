//! The new-items watch (OJ-19): an owner command prints one item per line as identity, a tab and a
//! title; each identity not seen before is reported once. The mail watch is this kind with a
//! mail-search command.

use std::time::Duration;

use super::Tick;
use crate::config::owner::Items;
use crate::jobs::event::{Emit, EventKind, Severity, one_line};

const OUTPUT_CAP: u64 = 64 * 1024;
const LINES_MAX: usize = 100;
const TITLE_MAX: usize = 160;
/// Seen identities kept; the oldest are forgotten first.
const SEEN_MAX: usize = 5000;
/// Bound of the owner command, inside the tick's own deadline (OJ req. 48).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) async fn tick(t: &mut Tick<'_>, items: &Items) -> Result<(), String> {
    let limit = COMMAND_TIMEOUT.min(t.job.timeout);
    let done = t
        .command(&items.argv, Some(&items.cwd), &[], limit, OUTPUT_CAP)
        .await?;
    if !done.success() {
        return Err(format!(
            "the command failed ({:?}): {}",
            done.exit,
            super::said(&done)
        ));
    }
    let text = String::from_utf8(done.stdout)
        .map_err(|_| "the command printed non-UTF-8 text".to_string())?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    if lines.len() > LINES_MAX {
        return Err(format!("the command printed more than {LINES_MAX} lines"));
    }
    let mut parsed = Vec::with_capacity(lines.len());
    for (n, line) in lines.iter().enumerate() {
        let Some((id, title)) = line
            .split_once('\t')
            .filter(|(id, _)| !id.trim().is_empty())
        else {
            return Err(format!(
                "line {} is not an identity, a tab and a title",
                n + 1
            ));
        };
        parsed.push((id.trim().to_string(), title.trim().to_string()));
    }
    let now = t.now.to_string();
    for (id, title) in parsed {
        if t.memory.get(&id, "seen").is_some() {
            continue;
        }
        t.memory.transition(&id, "seen", &now);
        if !t.first {
            let name = t.job.name.clone();
            t.emit(
                Emit::new(EventKind::ItemNew, Severity::Attention, true, &id)
                    .with("job", name)
                    .with("id", &id)
                    .with("title", one_line(&title, TITLE_MAX)),
            );
        }
    }
    let mut seen = t.memory.with_condition("seen");
    if seen.len() > SEEN_MAX {
        seen.sort_by_key(|(_, at)| at.parse::<i64>().unwrap_or(0));
        let excess = seen.len() - SEEN_MAX;
        for (id, _) in seen.into_iter().take(excess) {
            t.memory.clear(&id, "seen");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::jobs::owner::tests::live_tick;
    use crate::jobs::tests::ctx_with;

    #[tokio::test]
    async fn new_items_are_reported_once_after_the_baseline() {
        let dir = crate::sys::fs::tests::scratch("items");
        let list = dir.join("list");
        std::fs::write(&list, "1\tHello\n2\tOld news\n").unwrap();
        let cfg = format!(
            "[jobs.mail]\nkind = \"items\"\nargv = [\"cat\", \"{}\"]\n",
            list.display()
        );
        let (ctx, _stop, _dir) = ctx_with(&cfg, "items-ctx");
        assert!(
            live_tick(&ctx, "mail").await.is_empty(),
            "baseline reports nothing old"
        );
        std::fs::write(&list, "1\tHello\n3\tInvoice\n").unwrap();
        assert_eq!(live_tick(&ctx, "mail").await, ["items.new"]);
        assert!(live_tick(&ctx, "mail").await.is_empty(), "no repeat");
        let events = ctx.store.read(|r| r.events_after(0, 10)).await.unwrap();
        assert_eq!(events[0].text, "mail: Invoice");
        std::fs::write(&list, "no tab here\n").unwrap();
        assert_eq!(live_tick(&ctx, "mail").await, ["job.failed"]);
    }
}
