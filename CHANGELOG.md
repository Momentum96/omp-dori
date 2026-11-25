# Changelog

## 0.1.3 (Unreleased)

- The ops panel tries again to pin itself when a pin fails: right after the hub starts, then at
  most once an hour until it works, so granting the bot Pin Messages later is enough. While the
  bot may not pin, the panel footer says so. Once pinned, an unpin by hand is left alone until the
  hub restarts.
- A failed OMP probe is tried again by itself after 5 s, 15 s and 45 s, then every 5 minutes,
  instead of waiting until the OMP executable changes. Just after an OMP update the first start
  can take longer than the probe allows; the lead now starts as soon as a later probe works.
  `cataly status` and `cataly health` say when the next try runs.
- The limits a Discord form checks (number ranges, text lengths, required fields) are worded in
  your language (`[hub] language`), in the Fill in placeholder, a refused Fill in and a refused
  Submit, like the rest of the question card.
- Voice over WebRTC no longer stops at the start of a call. Setting up a WebRTC session takes the
  provider a few seconds, and voice used to give up as soon as more than 3 s of your audio was
  waiting; it now keeps the newest 3 s until the session is up and sends them then.
- `cataly update` and `cataly rollback` rewrite the user service Cataly created (systemd unit or
  launchd agent) with the definition of the version they switch to, and have the service manager
  read it again before they start the hub, so a new definition (such as the single malloc arena)
  needs no `cataly service enable`. The new `cataly service refresh` does this; a version from
  before it keeps the definition as it is.

## 0.1.2 (2026-11-26)

- The lead resumes after an update or restart even when its session was started in another folder
  than today's default: it reopens in the folder its session recorded. OMP's per-session folder
  next to the session file never blocks a resume.
- The message after an update and the `cataly update` line say how the lead came back: resumed, or
  started fresh with the reason. Lanes that run again are named.
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
- No more approval requests for Cataly's own tools: the lead and lanes call them directly, and
  OMP runs them without asking. A shell line that names them is refused at once with a pointer
  to the direct call, instead of asking you.
- Approvals follow your own OMP settings (decision 19): Cataly no longer forces OMP's
  `always-ask` and keeps no list of its own, so OMP asks exactly when your OMP configuration (or
  its default) says so. Read-only shell commands run without a prompt under OMP's default, and
  every dialog OMP does open still reaches you in Discord.
- The Discord approval card speaks your language (`[hub] language`): "권한 요청", the tool and the
  command in a code block, long commands cut cleanly, buttons "이번만 허용" / "거부".
- On a Discord question card, the option OMP marks as recommended says so in your language
  ("(추천)" or "(recommended)") on its button or menu option and in the card text; a long option
  is cut to fit, never the mark. Nothing is added when OMP recommends none.
- Typed answers to question cards: the hints and replies around them follow your language, and a
  card with several questions or fields takes a typed reply to one question's part (or to the
  only part still open), as an option number, a label or, where OMP allows, your own text; the
  other parts stay open and the menus still work.
- Setup's Discord invite asks for Pin Messages, so the ops panel can pin its message; without it
  the panel still posts and edits in place, unpinned.

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
