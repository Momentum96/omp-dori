# Changelog

## Unreleased

- `#cataly-ops` is a status panel instead of a stream of one-line notices: one pinned embed in
  your language with the host, usage per provider, the lead and lanes, job health and the last
  three events, edited in place at most once a minute. Owner-only buttons refresh it, show the
  full usage table and mute alerts for an hour. Alerts are separate small posts only for what is
  worth a ping (the host over the alert level for five minutes, a quota hold, a job failing twice,
  a refused backup, GitHub news, new mail), at most one per kind an hour, edited to "resolved"
  when the condition clears. `[ops]` sets the room, the mute length and which alerts post.
- The pressure job raises its alert after five minutes over the line, and ready, busy and alert
  states change only once the load crosses back 10 points, so a load near a line no longer
  flips them every tick.
- Setup also gives the Discord application Cataly's avatar as its icon, under the same rule as
  the bot's images: an icon you set yourself is kept until `cataly setup --profile`.

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
