# Changelog

## 0.2.4 (2026-11-27)

- The ops panel has a **📡 Watched channels** button. It opens a private Discord channel picker
  with the watched channels already chosen. On submit the hub checks that the bot may view and
  read the history of each newly picked channel (one call each) and refuses the whole pick,
  naming the channel and the missing permission, if not. Then it writes `[discord] channels`
  into the config file as it is at that moment (keeping your other lines, moving `home` to a
  watched channel when needed, dropping unwatched channels from `others`), checks it as
  `cataly config check` does and applies it as `cataly config reload` does. Only
  `[discord] owner` may use it, from `[discord] guild`; an invalid result or a failed write
  changes nothing. Channels past the picker's 25 are kept.
- A change of `[discord] channels`, `home` or `others` alone, from the panel or
  `cataly config reload`, now applies on the running Gateway connection: a new channel is read
  from the moment the change was applied and catch-up after a reconnect covers it; a dropped one
  stops being read at once. Before, any `[discord]` change restarted intake and opened a new
  connection.
- A watched channel the bot cannot read no longer stops Discord intake for every channel: it
  leaves intake alone with one `relay.held` notice naming the missing permission, at start, on a
  channel change or when a catch-up read meets it.
- The lead sleeps when it has been fully idle for `[lead] idle_sleep` (default `2h`; `0s` turns it
  off): no turn, no message waiting or in doubt, no open card or owner question and no open lane
  (quiet notes wait through the sleep and go with the first turn after it). The hub
  stops its OMP the orderly way and keeps the session; an owner message (typed or spoken), an
  urgent event, a lane report or `cataly lead send` resumes the same session, and the message is
  delivered once it is accepted. `cataly status`, `cataly lead status`, `cataly health` and the
  ops panel show `asleep`; an update keeps a sleeping lead asleep.
- The lead hands work to lanes. It does work likely to take 5 minutes or less itself in the
  channel; longer work, development in a repository and work worth running beside other work go
  to a lane in a thread under the owner's message, with a brief, done checks and a one-line
  kickoff (`lane.open` `kickoff`, posted first in the new thread), and the lead answers with one
  line naming the thread. An owner message in a lane's thread that asks to split a task out
  becomes a second lane in a new thread beside the first (Discord nests no threads, so
  `thread_from` a message inside a lane thread starts the thread in its channel).
- A lane opened with `worktree` now works in that worktree. Before, its session, its done checks
  and its brief's directory were the repository it was cut from, so lanes meant to edit in
  parallel all edited the main checkout.
- `cataly setup --voice on` turns on Discord voice calls in one command. It fills in every
  `[voice]` key the config does not set yet: a voice channel (`--voice-channel <id|name>`, else
  the server's only one, else `General`/`일반`, else a `Cataly` voice channel found or created),
  the lead's home channel for the text, the models, voice and prices the live voice test uses, the
  system time zone, the caps (30 minutes and 3 USD a day unless `--voice-daily-minutes` or
  `--voice-cap-usd` say otherwise) and the key file (`--voice-key-file`, else
  `secrets/voice-key`; without one setup stops before enabling anything). Keys you set are kept.
  The summary says that calls cost money on your OpenAI key and shows the caps. `--voice off` turns
  voice off and keeps the other keys. `cataly config check` now warns when voice is enabled but
  would stay off, and names why. Before enabling voice, setup checks that the bot may view, connect
  and speak in the voice channel; if not, it stops and prints a fresh invite link or names the
  channel overwrite to fix.
- The bot's invite link now also asks for Connect and Speak, which voice calls need
  (`permissions=2252126234414160`). A bot invited before keeps its old role until you open the new
  link.
- Usage no longer holds session starts. The `quota` job keeps its alerts (one warning per level,
  `levels` default `[70, 85, 95]`, and one note when a limit falls back below every level), and
  the ops panel keeps its usage bars and turns amber at 90 % for a provider a live session uses.
  `cataly quota`, `cataly quota lift`, the `quota.hold` and `quota.released` events and the
  held-lead lines of `cataly lead status`, the panel and the update message are gone; `cataly
  limits` shows usage. `alerts.quota` now posts a usage alert (`quota.level`).
- A quota job's `hold` setting no longer does anything. The hub removes its line at start, keeps
  the file as it was in `config.toml.before-0.2.4` and records a `config.cleaned` event; a file it
  cannot write keeps the line, ignored, and `cataly health` and `cataly config check` name it.
- An owner job's one-off server or network failure (HTTP 5xx, 429, a timeout) is retried on the
  next tick and becomes a `job.failed` event, and so a note for the lead, only when it happens
  twice in a row; a 404, 401 or bad setting is reported at once as before.
- The planner no longer logs "planner pass failed" when it skips a pass that is not due yet.
- On macOS the CLI now reports a control connection the hub closed without an answer as "the hub
  closed the connection", as on Linux, instead of "Invalid argument (os error 22)". macOS can
  report such a close as EINVAL or ENOTCONN rather than end of stream; these, like EPIPE and
  ECONNRESET, count as closed without an answer on every platform, so `cataly update` and
  `cataly rollback` count the stop once the hub has ended.
- An OMP that is slow to exit after end of input on a loaded host no longer fails the OMP probe
  when it ends at TERM; only a failed exit or one that needs KILL does. At hub shutdown each lane
  now gets the same 27 s as the lead, so a slow exit keeps its full TERM grace before any KILL.
- Voice no longer fails and holds itself right after a pass to the lead with "provider error
  unknown_parameter: Unknown parameter: 'delegation_id'". Continuing the backend after a tool
  result sent `delegation_id` on `response.create`, which takes only `type` and `event_id`. The
  fake realtime server in the tests now rejects every field the Live API does not document for an
  event, with the API's error shape.
- Lead replies are now spoken in a live call, and a quiet call closes after the idle time. The
  provider streams output audio all the time, silence included, and every chunk counted as
  output, so voice never saw a pause: no reply piece was offered and idle close never came. Only
  audible audio (at or above -55 dBFS) now counts as output.
- After a provider failure or a voice cap, the summary post says what the owner and the
  assistant said and what the tools did, instead of only "no summary: the session failed". No
  model call is made, and voice stays held until `cataly voice restart`.
- A quiet owner no longer logs "voice receive stalled; stream restarted" every 10 to 15 s. Discord
  sends no audio during silence and reports speaking once, so silence was taken for a stall; a
  stall is now owner audio that keeps arriving but does not decode.

## 0.2.3 (2026-11-27)

- `cataly update` and `cataly rollback` no longer report a stopping hub as held. A hub that
  accepts the orderly stop now writes its acceptance before it begins to stop; before, a hub
  that stopped quickly could close the connection first, and the update said "held: the hub did
  not stop ... nothing changed" while the hub ended and was not started again. A connection
  closed without an answer now counts as a stop only once the hub has ended within the minute,
  and the new hub is started only once the old one has released its lock.
- Installing or updating with `install.sh` no longer fails when Linux briefly refuses to start
  the staged binary for its version report ("Text file busy"); the start is retried for up to
  1 s.
- A lead in a Tern terminal no longer has a message put in doubt ("the bridge did not answer
  deliver within 10 s") when its session starts or compacts while a message is on its way. The
  hub handed the lead its recap on the bridge connection itself and read nothing more from it
  until then, so the bridge's answer waited behind a recap that waited for that answer. Seen on
  the first message after a start on slow Intel Macs.

## 0.2.2 (2026-11-26)

- The source and the releases now live together in vntrevx/Cataly.
- Releases for macOS and Linux on arm64 and x64: `linux-x64`, `linux-arm64`, `darwin-arm64`
  (Apple Silicon) and `darwin-x64` (Intel Macs). `install.sh`, `cataly update` and the npm and
  Bun packages (`cataly-<target>`) pick the binary for the host. Windows is not supported.
- On macOS, `cataly setup` makes a launchd agent (`~/Library/LaunchAgents/dev.cataly.hub.plist`)
  where Linux gets a systemd user unit.
- The store's bundled SQLite is 3.54 (rusqlite 0.41), past RUSTSEC-2026-0459.
- Cataly sends no telemetry. The opt-in usage counts and the `[usage]` config table are gone; a
  config that still has a `[usage]` table is refused, so delete it.

## 0.2.1 (2026-11-26)

- Cataly is now open source under AGPL-3.0-only, with commercial licences available from vntrevx;
  the source is public at https://github.com/vntrevx/cataly-core.
- The quota gate holds new sessions only, as documented. Resuming an existing session no longer
  waits for it: the lead's last session and parked or live lanes come back after a hub restart,
  `cataly update` or `cataly rollback`, and `cataly lead start` without `--fresh` resumes. A
  fresh lead (`lead start --fresh`, or a first lead with no session yet) and new lanes still wait.
- When the gate holds the lead, the hub says so: the `lead.refused` alert, `cataly lead start`,
  `cataly lead status`, the ops panel, the `cataly update` line and the update message (Korean
  and English) name the provider, the limits with their reset times and `cataly quota lift`.
- `cataly update` and the update message no longer wait out the full minute when the hub does
  not start the lead: a refused start is reported at once with its reason, and a hub that does
  not answer in time is reported instead of said nothing about.

## 0.2.0 (2026-11-26)

The first public release. Releases now come from `github.com/vntrevx/Cataly`, for Linux x64
(macOS arm64 follows once its build is published).

- Install with npm or Bun as well as `install.sh`: `npm install --global cataly` or
  `bun add --global cataly`. The package runs the same binary; the hub links its OMP extension
  when it starts, and `cataly setup` makes the user service as before.
- `cataly update` on an npm or Bun install updates through that package manager, with the same
  ordered stop, restart, wait and lead resume as an `install.sh` install; if the new version does
  not come up, the earlier one is installed again. The service it runs keeps working across
  upgrades. `cataly rollback` and `cataly uninstall` say which package-manager command to use.
- `install.sh`, `cataly update`, `cataly check` and the daily release notice take the newest
  stable Cataly release: prereleases, drafts and other software's releases in the repository are
  skipped, and a release without this host's files is refused with its name and what it lacks.
- Everything from 0.1.x carries over: one-command `cataly setup`, the lead and lanes resuming
  across updates and restarts, the `#cataly-ops` status panel and alerts, watch jobs, Discord,
  Telegram and Slack rooms, and Discord voice.

## 0.1.5 (2026-11-26)

- A held provider that no live session uses no longer turns the ops panel amber, and its spent
  limit no longer counts as a high limit: with nothing else wrong the panel is green, with the
  held-provider line and the per-provider ⛔ marks still shown.
- A usage limit turns the panel amber only at 90 % or more, not 70 %. `[quota] levels` alerts
  are unchanged.

## 0.1.4 (2026-11-26)

- The ops panel, `cataly limits` and quota events name every provider as OMP reports it, an
  extension provider such as `devin` included, instead of "unknown".
- The panel's session count includes the lead, and the `max_sessions` check on a lane start now
  counts the lead too, so both use the same number.
- A held provider that no live session uses turns the panel amber, not red, and a line under the
  summary names the held providers and says sessions on other providers start normally. The
  panel turns red when a live session's provider is held or the lead is held.
- The panel says when it was last updated as a relative time under the summary; the footer shows
  the version with the update time beside it.

## 0.1.3 (2026-11-26)

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
