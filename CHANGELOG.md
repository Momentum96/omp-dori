# Changelog

## Unreleased

- No more approval requests for Cataly's own tools: the lead and lanes call them directly, and
  OMP runs them without asking. A shell line that names them is refused at once with a pointer
  to the direct call, instead of asking you.
- Read-only shell commands (`ls`, `cat`, `grep`, `head`, `git status`, `git log`, `git diff` and
  the like) run without asking; anything that writes, deletes, pushes, installs or uses the
  network still asks (coordinator decision P1, pending your confirmation).
- The Discord approval card speaks your language (`[hub] language`): "권한 요청", the tool and the
  command in a code block, long commands cut cleanly, buttons "이번만 허용" / "거부".

## 0.1.1 (2026-11-26)

- One-command setup: `cataly setup` runs the whole first run (owner-only folders, the Discord
  steps, defaults, the home backup, the user service) after one summary and one yes; a re-run
  changes nothing, and a home backup you turned off stays off.
- Bot avatar and banner: setup gives the bot Cataly's images and keeps ones you set yourself.
- Per-provider quota gate: a provider near its quota holds only the work that uses it.
- The lead's tool is `cataly_lead`, so OMP's tool command no longer shadows the `cataly` CLI and
  the lead can record through it from its shell.
- Update notice: a newer release is announced once in your Discord home room, in your language
  (`[hub] language`, `ko` or `en`), with an optional `[update] template`.
- Plain `cataly update` works while the hub runs: it stops the hub in order (never a busy
  session; after 60 s it says which session it waits for), switches, starts the new hub the way
  the old one ran and switches back if it does not answer within 30 s. The lead and lanes resume
  on their own session files with their history and goals. `--no-restart` installs without
  touching the hub; `cataly rollback` restarts the same way.
- After an update or a rollback the hub says so once in your home room, with the sessions it
  resumed (`[update] updated_template` changes the update's wording).
