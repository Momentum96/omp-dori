# Owner jobs: behaviour spec

Behaviour spec used to write Cataly 0.2; see docs/provenance.md.

## Purpose

The owner runs a set of personal watchers beside the product: a GitHub watch over his own pull requests, a quota
watch that pauses new work near a limit, a model watch that notices a silent switch to another model and switches
back, a watchdog that refuses any offer to spend paid quota, a backup of his working state to a private git
repository, a self health check, a mail watch, a "while you were away" digest, a tracker of unanswered messages,
and a handful of messenger helpers. Today each is a separate shell, Python or TypeScript process that loops, scrapes
a terminal multiplexer's screen and prints wake lines. Sessions are now headless, so the screen-scraping watchers see
nothing for headless sessions.

This spec turns them into native jobs of the one Rust daemon, all fitting one job contract (section A). Everything
personal (repositories, logins, channels, thresholds, time zone, paths, commands) is configuration. Users: the owner
(configures, receives messenger posts, may override holds), the lead session (receives events, may preview and run
jobs once in dry-run), sessions (are observed; never control jobs), and the daemon (runs every job).

This spec relies on the event stream, transition memory, event log, pull-request follower, condition jobs, account
limits view and health report of the watch-schedule area (WS-xx), on dialogs and model change in session-host
(HOST-xx), and on lane admission in lane-lifecycle (LANE-xx). It states only what it adds.

## Capabilities

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| OJ-01 | Common job contract: cadence, state, change detection, once per transition, targets, dry-run | all jobs | SIMPLIFY | One contract on WS-01 instead of ten hand-written loops. |
| OJ-02 | Shared append-only event log written by every watcher, read by the digest | digest, owner | MERGE into WS-01 | The daemon's event log already exists. |
| OJ-03 | Wake lines printed on stdout and matched by the lead's filters | lead | MERGE into WS-01 | Typed events replace text matching. |
| OJ-04 | Per-script dry-run switches in environment variables | owner | MERGE into OJ-01 | One dry-run flag per job. |
| OJ-05 | Watch the owner's PRs on a repository (new, merged, closed, reviews, comments, failed checks) | owner | MERGE into WS-25 | Same follower; add discovery by author. |
| OJ-06 | GitHub mentions and review requests from notifications | owner | KEEP | Cheap, owner-visible, not covered elsewhere. |
| OJ-07 | Quota level alerts for short and weekly windows | owner, lead | SIMPLIFY | Read OMP's usage output, not a status line. |
| OJ-08 | Quota hold that defers new sessions | lead, admission | KEEP | Protects remaining quota; in-daemon flag, not a file. |
| OJ-09 | Quota reader keyed by limit identity | usage job | MERGE into WS-23 | One account-limits reader. |
| OJ-10 | Detect a session running on another model than requested, or on none | owner, lead | SIMPLIFY | Use OMP's fallback events and serving model; covers lead and lanes. |
| OJ-11 | Return the lead to its requested model after a fallback | owner | SIMPLIFY | Model change on an idle session; no probe prompt, no restart. |
| OJ-12 | Model-family mismatch check on lanes | lead | MERGE into OJ-10 | Exact comparison replaces family guessing. |
| OJ-13 | Decline paid-quota offers in any session automatically | safety | KEEP | Never-spend rule; event-driven, fail closed. |
| OJ-14 | Notice a session stopped on another dialog or blocked | lead | MERGE into WS-15 | Already specified. |
| OJ-15 | Back up a state directory to a private git repository with a secret scan | owner | SIMPLIFY | Keep; alert once per transition, bound every git call. |
| OJ-16 | Self health check: process list, sockets, heartbeat file, two-strike rule | owner | SIMPLIFY | Daemon-internal checks; keep two-strike and direct post. |
| OJ-17 | Check that the upstream updater service runs | owner | DROP | The updater is gone. |
| OJ-18 | Warn when the lead's continuity note has not changed for hours | owner | KEEP | Cheap; one configurable file-age check. |
| OJ-19 | Mail watch: report each new mail from a sender once | owner | SIMPLIFY | Generic new-items job over an owner command. |
| OJ-20 | Away digest of chats, PRs, session history and alerts | owner | SIMPLIFY | Built from daemon records and bot-readable rooms. |
| OJ-21 | Detect the owner's return after a long silence | digest | KEEP | Trigger for OJ-20. |
| OJ-22 | Track messages from others left unanswered | owner | SIMPLIFY | Keep; only in rooms the bot can read. |
| OJ-23 | Read group DMs and search mentions with the owner's personal account | owner | DROP | Self-bot use breaks Discord's terms; account risk. |
| OJ-24 | Post as the owner with his personal account | lead | DROP | Same risk; the product speaks only as its bot. |
| OJ-25 | Bot send helper with per-channel lock and spacing | lead | MERGE into discord outbound | The daemon is the only sender and serialises. |
| OJ-26 | Pre-send text checks (unfilled slots, placeholder words, banned characters) | lead | SIMPLIFY | Keep as configurable checks on outgoing text. |
| OJ-27 | Verified reply helper | lead | MERGE into discord outbound | Replies are already native. |
| OJ-28 | Open a work thread and add the owner as a member | lead | MERGE into discord outbound | Add owner membership to thread creation. |
| OJ-29 | Inbound poller, per-room session router and its on-disk queue | lead, lanes | MERGE into discord inbound | Gateway intake replaces polling and the queue. |
| OJ-30 | Typing indicator while the lead works on an owner request | owner | MERGE into discord outbound | Owner uses it; drive it from session state. |
| OJ-31 | Keep the bot online | owner | MERGE into discord outbound | Same Gateway connection. |
| OJ-32 | Voice note transcription | lead | MERGE into voice area | One speech-to-text path. |
| OJ-33 | Restart helpers: idle-streak restart, detached restart, launcher wrappers, second-persona relaunch | owner | DROP | Session-host owns restart and model change. |
| OJ-34 | Exchange listing lookup across trading venues | owner | DROP | A personal research tool, not a job; stays outside the product. |

## Requirements

### A. Job contract (OJ-01, OJ-04)

1. An owner job is a built-in job kind plus a configuration instance. The kinds are: GitHub watch, quota watch,
   model watch, spend-offer guard, backup, health, new-items watch, digest, unanswered-message watch. Several
   instances of a kind may exist (for example two backups), each with a unique name.
2. Every instance has: an enabled switch, a cadence (each kind states its default and minimum), a per-tick
   deadline, a dry-run flag, a list of delivery targets, and its kind's settings. Unknown keys, a cadence below the
   minimum, an unknown target or a missing required setting make the configuration invalid; an invalid file is not
   applied and the previous one stays in force (WS-35).
3. A tick reads its sources, computes the current set of conditions, each a (subject, condition, detail) triple,
   and compares it with the stored set. Kinds use one of four change rules: state difference (an item appeared or
   one of its fields changed), new item (an identity not seen before), level crossing (a value moved into a higher
   or lower band) and health (a problem confirmed or cleared).
4. Report once per transition (WS-01 rule 4). Exceptions: an action the daemon took (a declined spend offer, a model
   switch, a backup refused) is reported every time it happens; a kind may require N consecutive bad ticks before
   a problem counts (health: N = 2 by default, 1 to 5).
5. First run: a tick with no stored state records a baseline and reports nothing that already existed, except
   problems found by health and the spend-offer guard, which report at once.
6. State is saved atomically after the tick's events have entered the stream. A crash may repeat an event; it never
   loses one. State for subjects that no longer exist (a closed session, a removed repository) is deleted.
7. Targets: the lead (urgent or deferred, per event kind), one or more messenger destinations (Discord channel or
   thread, Telegram chat, Slack channel), and the subject's own thread when the subject is a session that has one.
   Every event also goes to the daemon's event log, whatever the targets. Messenger posts never mention anyone.
8. A failed tick emits one warning per distinct reason and leaves state untouched; the next tick tries again.
   An overrun of the per-tick deadline cancels the tick, is reported once, and the next tick runs on schedule.
9. At most one external command runs per job at a time, and at most four job commands run across the daemon. Each
   command has an argument vector (no shell), a timeout, an output cap, and is killed (TERM, KILL after 5 s) on
   expiry.
10. Dry-run: the tick runs fully, its events are written to the event log and given to the lead marked as dry, and
    no messenger post is made and no action is taken (no model change, no commit, no push, no hold). Dry-run state
    is kept apart and discarded when the job goes live; the first live tick takes its own baseline.
11. "Run once": the owner (local command) or the lead may run one dry tick of any job now and see its events.
12. Authority: only the owner changes job configuration (file or local command). The lead may list jobs, run them
    once in dry-run and read their state. Sessions other than the lead can do none of this. The spend-offer guard
    cannot be disabled or set to dry-run by anyone.
13. Event texts have default English wording. The owner may override the wording per event kind with a template
    that uses named fields; a template naming a field the kind does not provide makes the configuration invalid.
14. The whole job set must add less than 5 MiB to the daemon's resident memory and spawn only `gh`, `git`,
    `omp usage` and owner-configured commands, never a resident helper process.

### B. GitHub watch (OJ-05, OJ-06)

15. Settings: a list of repositories, each with the author logins whose PRs are watched; bot logins whose comments
    are ignored; destinations. Cadence default 3 minutes, minimum 1 minute.
16. Discovery: each tick lists the most recent 40 PRs per (repository, author), in any state, through `gh`.
    A PR not yet followed is followed by the WS-25 follower with this job's targets. After the baseline, a newly
    discovered PR is reported once with its number, title (cut to 60 characters) and state.
17. Everything after discovery (reviews, comments, failed checks, merge, close, reopen) is the WS-25 follower's
    work and its rules. Comments and reviews by the watched author and by configured bots are not reported.
18. Notifications: each tick reads the authenticated user's participating notifications (at most 50 per tick) and
    keeps those for a configured repository with reason mention, team mention or review requested. Identity is the
    notification thread plus its update time; each new identity is reported once (reason and subject title cut to
    80 characters). Remembered identities older than 30 days are forgotten.
19. Each `gh` call has a 60 s timeout. A failure of either read leaves all state of that tick untouched.

### C. Quota watch and hold (OJ-07, OJ-08)

20. Source: OMP's usage command with JSON output (WS-37 limits and caching). Cadence default 5 minutes, minimum 2.
    Each limit is identified by provider, anonymous account ordinal and the limit's own identity, never by its
    window's display name: two limits can share a window name.
21. Settings: providers to watch (default: the providers of the lead's and open sessions' requested models), alert
    levels (default 70, 85, 95 per cent), hold level (default 85), destinations.
22. Each limit has a band: the highest alert level its used share reaches. A rise to a higher band emits a warning
    with the share, the window length and the time until reset. A fall from at or above the hold level to below it
    emits one recovery note. Other falls are silent.
23. Hold: while any watched limit is at or above the hold level, the daemon holds new session starts. The hold
    ends when fresh data shows every watched limit below the hold level, or when every limit that caused it has
    passed the reset time OMP reported, whichever comes first. Unavailable data never extends the hold past those
    reset times, so a hold cannot strand work for ever.
24. While held, a request to open a lane is accepted into lane admission's pending list (LANE) and starts in order
    when the hold ends; running sessions continue untouched; the lead is told once per hold with the reason.
25. The owner may lift a hold for one named launch or until the current window resets. The lead and other sessions
    cannot lift it.
26. Unavailable or malformed usage data emits one warning per distinct reason and keeps the last known bands.
27. The job never runs a usage subcommand other than the read, never spends a saved reset, never switches account,
    provider or model.

### D. Model watch (OJ-10, OJ-11, OJ-12)

28. Subjects: the lead and every open session. Signals: OMP's retry-fallback-applied and fallback-succeeded
    events, the session's serving model in its state answer, and the model on its newest assistant message in the
    session file. Checked on each such event and on a 60 s timer.
29. A session is "off model" when its serving model differs from its requested model (provider exact, model ID
    case-insensitive), and "no model" when it has none. One event per distinct observed model; one recovery note
    when the requested model serves again.
30. Recovery applies to the lead, and to lanes only if the owner enables it per lane. Every 10 minutes while off
    model: if quota data shows a limit of the requested provider exhausted, wait until its reset time and do
    nothing else; otherwise wait until the session has been idle for 30 s with no dialog and no queued input,
    post one note to the destinations, and change the model through session-host (HOST-18) with readback.
31. Success is decided by the next assistant message: the requested model means recovered; another model means
    the attempt failed, and the next attempt waits twice as long (10, 20, 40 minutes, at most 2 hours).
32. The daemon never changes a session to any model other than its requested one and never restarts a session to
    recover. If OMP itself returns to the requested model first, only the recovery note is sent.

### E. Spend-offer guard (OJ-13)

33. Every session the daemon starts gets OMP's `codexResets.autoRedeem` set to `no` in its overlay configuration,
    so OMP does not offer or perform saved-reset spending on its own.
34. Every dialog from a headless session (HOST-14) is classified as soon as its frame arrives, and again on a 60 s
    sweep of pending dialogs. A dialog is a spend offer when its title, message or any option matches a spend
    phrase: built-in phrases (saved reset, rate-limit reset, redeem, extra usage, overage, buy or purchase
    credits, top up, upgrade plan) plus owner-added ones. Built-in phrases cannot be removed.
35. A spend offer is answered with cancel within 1 s, never with "no" or a remembered choice, so no preference is
    stored. The daemon then expects the dialog's removal within 5 s; if it stays, it cancels once more, holds the
    session's input, and sends an urgent alert to the lead and the owner's destinations.
36. Every declined offer is reported (session, time, matched phrase), to the session's thread or the default
    destination, and to the lead. Three offers from one session within 10 minutes hold its input and alert once.
37. Sessions in the terminal runtime show dialogs to the person at the terminal; the daemon cannot answer them and
    sends an urgent alert to the owner's destinations when OMP reports such a dialog.
38. The daemon never runs OMP's reset-spending usage subcommand. The lane command guard (LANE) blocks it in every
    session as a non-overridable rule.

### F. Backup (OJ-15)

39. Settings: the directory (a git working tree), remote and branch, commit name and e-mail, extra secret patterns,
    maximum file size (default 50 MiB). Cadence default 30 minutes, minimum 5. What is excluded is the tree's own
    ignore file.
40. The configuration is invalid if the daemon's own secret store or configuration file lies inside the directory
    and is not ignored.
41. Tick: stage every change; if nothing is staged, end. Scan the staged content with the built-in token shapes
    (GitHub classic, OAuth and fine-grained tokens, Slack bot and user tokens, Discord bot tokens, private key
    blocks) and the owner's patterns. Any hit, or any staged file over the size limit, unstages everything and
    emits one alert naming the files and the rule, never the matched text. The alert repeats only when the set of
    files changes; the first clean commit afterwards emits a recovery note.
42. Clean: commit with a timestamped message and the configured identity, then push to the configured branch.
    Never force-push, rewrite history or pull. A failed push is reported once per distinct reason; the commit stays
    local and the next tick pushes it.
43. A git lock held by someone else skips the tick with one warning. Timeouts: 60 s per local git call, 300 s per
    push.

### G. Health (OJ-16, OJ-18)

44. Cadence 2 minutes. Problems checked: each messenger connection the daemon should hold is not established;
    the lead session is not bound or not responding to a state request within 10 s; a job's last good tick is
    older than twice its cadence; the event backlog for the lead is growing with the lead disconnected for more
    than 10 minutes; a configured file (default the lead's continuity note) unchanged for longer than its age
    limit (default 12 hours) or missing.
45. A problem counts after two consecutive bad ticks; it is reported once and its clearing once.
46. Health events go straight to the owner's destinations as well as to the lead, so the owner hears of them when
    the lead itself is stuck.
47. Liveness of the daemon itself: under systemd it uses the service watchdog (notify every 10 s, watchdog 30 s);
    under launchd it uses keep-alive plus an internal stall detector that exits the process when the main loop has
    not advanced for 60 s. After a restart caused by a stall, one note is posted on the next start.

### H. New-items watch (OJ-19)

48. Settings: an owner command as an argument vector, cadence (default 30 minutes, minimum 5), timeout (default
    60 s), destinations. The command prints one item per line as an identity and a title separated by a tab, at
    most 100 lines and 64 KiB. Other output is a failed tick.
49. Each identity not seen before is reported once with its title (cut to 160 characters). Seen identities are
    kept up to 5,000; the oldest are forgotten first. The mail watch is this kind with a mail-search command.

### I. Owner return and digest (OJ-20, OJ-21)

50. The owner has returned when his first message in any room the bot reads arrives three hours (configurable) or
    more after his previous one. The time of his latest message survives restarts.
51. On return, and on request by the owner or the lead with a start time, the daemon builds a digest since the
    previous owner message (or the given time), in the configured time zone:
    a. per watched room: counts of messages from others and from the owner, then each message with local time,
       author name, text cut to 160 characters and attachment names; at most 500 per room; an unreadable room
       gets one line saying so and does not stop the others;
    b. GitHub events from the event log in that span;
    c. session status changes in that span with their notes;
    d. warnings and errors from all other jobs in that span.
52. The digest goes to the lead as one urgent event (body over 64 KiB as a private file). The lead turns it into
    the briefing; the daemon posts nothing itself.

### J. Unanswered messages (OJ-22)

53. Settings: rooms the bot can read that hold conversations with other people, alert age (default 120 minutes),
    drop age (24 hours).
54. A message from anyone other than the owner and the bot opens an entry. A message from the owner or the bot in
    that room closes every earlier entry in it. An entry older than the alert age is reported once (room, author,
    age, excerpt of 140 characters, link); entries older than the drop age are removed silently.

### K. Messenger helpers kept (OJ-26, OJ-28, OJ-30)

55. Outgoing text checks, applied to every text the daemon posts for the lead or a job: refuse text containing an
    unfilled template field (a name in braces), a placeholder word from a configured list, or a configured banned
    character (the owner's list includes the em dash). A refused post returns the reason and sends nothing.
56. When the daemon creates a work thread it may add the owner as a thread member, so the thread shows in his
    sidebar (on by default).
57. While the lead works on an owner request from a Discord room, the daemon sends the typing signal there every
    8 s, starting at turn start and stopping at turn end, an error, or after 10 minutes.

## Third-party interfaces used

- OMP: rpc-ui dialog frames and answers (extension_ui_request, cancel), get_state serving model, set_model,
  retry_fallback_applied and retry_fallback_succeeded events, session files (assistant message model), the
  `omp usage --json` read, overlay settings `codexResets.autoRedeem`, `retry.modelFallback`, `retry.fallbackChains`.
- `gh`: `pr list` with `--author`, `--state all`, `--json`; `api` on `/notifications` with participating only.
- git: add, diff of the index, grep of the index, commit, push, reset of the index.
- Discord REST: create message, start thread, add thread member, trigger typing; read channel messages.
- systemd: Type=notify with WatchdogSec and sd_notify; launchd: KeepAlive.

## State that must survive a restart

- Per job: baseline taken, stored condition set, consecutive bad counts, last good tick, distinct failure reasons
  already reported, dry-run state kept apart.
- GitHub: followed PRs (WS-25 state) and seen notification identities with their time.
- Quota: band per limit, hold on or off with its causing limits and their reset times, owner lifts.
- Model watch: per session the observed model already reported, next recovery time and backoff step.
- Spend guard: recent declines per session (for the three-in-ten-minutes rule).
- Backup: last reported hit set and last push failure reason.
- New items: seen identities. Digest: time and room of the owner's latest message. Unanswered: open entries.

## Problems in the old approach, and better ideas

- Ten separate loops (five shell, two Python, one TypeScript poller plus a router and a presence keeper) at about
  100 MiB resident between them, before the old product's own four watchers of 34 to 40 MiB each. Each tick also
  spawned multiplexer, `gh`, Python and YAML helpers, and the model watch spawned a full OMP for each probe.
  Better: jobs inside one daemon (A.14).
- Quota, model and session watchdogs read text off a terminal multiplexer's screen. Headless sessions have no
  screen, and the session watchdog skips any session without a pane, so today the never-spend guard does not
  protect headless sessions at all. Better: dialog frames and state answers (D, E).
- The quota watch read a short-window meter that only one provider prints on the status line and a weekly
  reading hard-wired to that provider, while the lead now runs on another provider; it also reads a different
  pane from the one the model watch is told to use. Better: OMP's usage output for the providers actually in use.
- The quota flag is a file whose freshness is judged by modification time, deleted after 15 minutes without data
  so the model watch is not stranded. Better: release at the reported reset time (C.23).
- The model watch proved recovery by running a throwaway OMP prompt in a temporary directory and diffing the
  session directory, then restarted the lead through a detached service-manager unit. Better: model change on
  an idle session and judge by the next real answer (D).
- OMP's own model fallback can move a session to another provider when a quota runs out, which the owner's
  never-spend rule forbids. See question 1.
- The backup re-alerted every 30 minutes while a secret stayed staged, and its push-failure test inspected output
  text. The health check hard-coded process names, including watchers that no longer exist and the dropped updater.
- Every channel, repository, login, time zone and threshold is hard-coded in the scripts; the old settings file has
  none of them. Messages are in one hard-coded language.
- Group DMs and mention search use the owner's personal account token, and some posts go out as him. Discord's
  terms forbid automating user accounts; a ban would cost the owner his account.
- Messages were posted with spacing and file locks per channel from many processes, and an extra file queue moved
  room messages into a session by typing them into a pane. The daemon is the only sender and receiver.

## Questions for the owner

1. OMP can fall back to another model or provider on errors, including exhausted quota. Should the daemon turn
   OMP's model fallback off for every session (this spec's safe reading of the never-spend rule), allow it only
   for outages, or leave it as configured today?
2. Reading your group DMs, searching your mentions and posting as you all need your personal Discord token, which
   Discord's terms forbid. Drop them (this spec), or keep them as an opt-in you accept the risk for?
3. Should the quota hold also pause scheduled jobs that start sessions, and should its default level stay at 85 %?
