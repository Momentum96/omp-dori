# Changelog

## Unreleased

- The lead resumes after an update or restart even when its session was started in another folder
  than today's default: it reopens in the folder its session recorded. OMP's per-session folder
  next to the session file never blocks a resume.
- The message after an update and the `cataly update` line say how the lead came back: resumed, or
  started fresh with the reason. Lanes that run again are named.

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
