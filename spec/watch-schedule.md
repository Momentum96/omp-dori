# Watching, timed jobs and health: behaviour spec

Behaviour spec used to write Cataly 0.2; see docs/provenance.md.

## Purpose

Everything Cataly does on its own while nobody is typing: noticing that a lane has gone quiet or is stuck,
enforcing a lane's time and spend limits, counting what lanes cost, showing account limits, following pull
requests after their lane has moved on, running timed and condition-triggered jobs, reporting health after a
restart and on demand, and carrying all of these notices to the lead session (and, where configured, to a
messenger thread) without repeating them.

Users: the lead session (receives notices, starts and stops background jobs, reads reports), the owner (defines
timed jobs, limits and settings, reads health reports), lanes (are observed; never control watchers), and the
daemon (runs every job). Lane records, admission and host-capacity rules, completion claims and death detection
are specified in the lane-lifecycle area; the report log and lead-to-lane message delivery are in the
evidence-reporting area. This spec only states what it needs from them.

Today each watcher is its own long-lived command whose text output is filtered by regular expressions inside the
lead's OMP session. The new model: **one Rust daemon, one job table, one typed event stream** (section A).

## Capabilities

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| WS-01 | Watchers as separate long-lived processes printing tagged text lines | lead | SIMPLIFY | Become jobs in one daemon emitting typed events; fewer processes, no text protocol. |
| WS-02 | Lead-side tool to run a command in the background and alert on matching output lines | lead | SIMPLIFY | Keep as a thin extension tool over daemon stream jobs; one restart policy instead of three modes. |
| WS-03 | Low-priority notices that wait for the next turn versus notices that start one | lead | MERGE into WS-01 | A field on each event, not a hard-coded pattern list. |
| WS-04 | Restore background commands on session resume from session entries and a copy on disk | lead | SIMPLIFY | The daemon persists its own jobs; no replay of session entries. |
| WS-05 | Per-second health file proving watcher children are alive by process ancestry | health report | DROP | The daemon owns its jobs and knows their state. |
| WS-06 | Self-test that each watcher's filter expression still matches every event shape | health report | DROP | Typed events need no regex coverage test. |
| WS-07 | Recommended watcher command lines and filters | lead | DROP | Built-in jobs need no presets. |
| WS-08 | Find watchers left on finished lanes by parsing shell command lines | start-up summary | DROP | Jobs carry an explicit lane link and stop when the lane closes. |
| WS-09 | Messenger intake hand-off over the background command's IPC channel | messengers | MERGE into discord-inbound | Intake lives in the daemon; not a watcher concern. |
| WS-10 | Prompt a silent lane for a progress line | lead | SIMPLIFY | Event-driven idle detection from OMP; one prompt per silence spell. |
| WS-11 | Send a lane's latest report to its messenger thread after a quiet period | owner | SIMPLIFY | Daemon sends through its own messenger client; keep the policy, drop command templates. |
| WS-12 | Read report lines from terminal screens, with a two-sample hold and template filters | lead | DROP | Screen scraping; reports come only through the report tool. |
| WS-13 | Notice that a held lane is working again | lead | MERGE into LANE-18 | Already specified in lane-lifecycle. |
| WS-14 | Notice that the thing a blocked lane waits on has ended | lead | KEEP | Cheap, useful, one notice per wait. |
| WS-15 | Notice that a lane is held by a pending dialog | lead | KEEP | Owner-visible; once per distinct set of dialogs. |
| WS-16 | Stuck-lane detection (busy a long time with no fresh evidence) | lead | SIMPLIFY | Use OMP's busy-since time, not an on-screen timer. |
| WS-17 | Evidence sampling from git status, declared files, submodules and side-effect path bookkeeping | stuck detection | SIMPLIFY | One bounded fingerprint per sample; drop the window and re-sample machinery. |
| WS-18 | Per-lane ceilings: wall-clock minutes, tokens, cost | owner | KEEP | Core spend safety; enforce, do not only advise. |
| WS-19 | External spend-reader command and external cost-recording command | owner | DROP | OMP session files are the one source; one implementation per concern. |
| WS-20 | Token and cost totals per lane from OMP session files, including subagents | owner, ceilings | SIMPLIFY | Keep the counting rules; read incrementally in the daemon instead of re-parsing whole trees. |
| WS-21 | Final spend reading recorded once per lane run at close | owner | KEEP | Stable history; trivial. |
| WS-22 | Spend report grouped by lane, thread or day | owner | KEEP | Small read-only view over WS-20. |
| WS-23 | Show provider account limits (rolling windows) | owner | KEEP | Read-only, bounded, no account changes. |
| WS-24 | Follow and stop following pull requests; list them; follow links from completion evidence | lead | KEEP | Small; automatic following stays opt-in. |
| WS-25 | Pull-request follower: new feedback, failed checks, foreign pushes, merge, close, reopen | lead | KEEP | Owner-visible with careful dedupe; one daemon job. |
| WS-26 | Timed jobs: cron with time zone, one-shot instant | owner, lead | KEEP | Native replacement for the owner's timers; strict no-replay rules are good. |
| WS-27 | Condition jobs: probe every N seconds, act on a change to success | owner | KEEP | The native form of the owner's personal watch scripts. |
| WS-28 | Per-run time limit for job commands | owner | KEEP | Every external call needs a bound. |
| WS-29 | Job result destinations: keep locally, give to the lead, send to a messenger | owner | KEEP | Single-attempt delivery with receipts. |
| WS-30 | Preview a job's next five trigger times without saving | owner | MERGE into WS-26 | An option on job creation. |
| WS-31 | Adopt external systemd timers for read-only display | owner | DROP | Native jobs replace them; Linux-only; not Cataly's job. |
| WS-32 | Separate job-runner process with an ownership marker and a file writer lock | runner | SIMPLIFY | The daemon is the only runner; its single-instance lock covers it. |
| WS-33 | Read-only start-up summary after a restart | lead | SIMPLIFY | One status view produced by the daemon, also on demand. |
| WS-34 | Health report with severities, ignore list and exit status | owner | SIMPLIFY | Keep useful checks; drop patch, receipt and filter-probe checks. |
| WS-35 | Configuration file, environment overrides, defaults, strict validation | everyone | SIMPLIFY | One file, few overrides, reload without restart. |
| WS-36 | Host pressure notices and compute-ready notices | lead | MERGE into LANE-09 | Specified in lane-lifecycle; runs as a WS-01 job. |
| WS-37 | Admission check command and slots for expensive commands | lead, lanes | MERGE into LANE-08, LANE-10 | Specified in lane-lifecycle. |
| WS-38 | Dead terminal session detection by screen markers | lead | MERGE into LANE-26 | Lane-lifecycle replaces markers with process events. |
| WS-39 | 30-second loop checking completion claims and queued launches | lead | MERGE into LANE-21, LANE-11 | Event-driven in the daemon. |
| WS-40 | Messenger sign-in check during the start-up summary | lead | MERGE into WS-33 | One line of the summary. |
| WS-41 | Command templates used to send to threads | owner | DROP | The daemon has native messenger clients. |

## Requirements

### A. Jobs and the event stream (WS-01, WS-03, WS-36, WS-39)

1. The daemon keeps one job table. Built-in jobs: lane oversight (B, C), pull-request follower (E), host pressure
   (LANE-09), the timed-job runner (F). No watcher is a separate process; only user job commands are spawned.
2. Every job reports through one event type: time, job, subject (a lane, a PR, a job name, the host), kind,
   severity (info, attention, warning, error), one-line text (at most 400 characters), optional long body, lane and
   thread links when known, and an urgent flag.
3. Urgent events start a lead turn; other events wait for the lead's next turn. Routine events (job started, a
   progress prompt that arrived, a pull request's first read) are not urgent; anything needing action is.
4. Transition rule for all watchers: for each (job, subject, condition) the daemon stores the last emitted
   condition and emits only when it changes. A repeated failure emits once per distinct reason; the first success
   after a failure emits one non-urgent recovery note. This memory is persisted, so a restart repeats nothing.
5. Routing: the daemon sends events to the lead through the Cataly OMP extension, which hands them to OMP with the
   extension API's send-message call (trigger a turn when urgent, otherwise deliver on the next turn). Events
   arriving within 300 ms form one message of at most 100 events or 64 KiB; a longer body is written to a private
   file (mode 0600) and the message carries its path and size.
6. If the lead is not connected, events are kept in order in a persistent backlog (at most 1,000 events or 7
   days; on overflow the oldest go and one event states how many). On reconnect the backlog is delivered oldest
   first in batches.
7. Events are also appended to a rotating log the owner can read (last 10 MiB), independent of delivery.
8. A failing or overrunning job does not stop other jobs. Each built-in job has a per-tick deadline equal to its
   cadence (at most 60 s); an overrun is reported once and the next tick is skipped, not queued.
9. Cadences: lane oversight on every relevant OMP event and on a 60 s timer; pull requests every 3 minutes; host
   pressure every 60 s; the runner sleeps until the next due instant (recomputed on any job change and on a
   wall-clock jump of more than 30 s).
10. Jobs linked to a lane stop when that lane closes or is suspended; the lead is told once which jobs stopped.

### B. Silent lanes and report sending (WS-10, WS-11, WS-14, WS-15)

11. "Last heard" for a lane is the latest of: when it was opened, the lead's last message to it, its last report.
12. Progress prompt. When all hold, the daemon sends the lane one short lead-voiced message asking for a one- or
    two-sentence progress line (what is finished, what comes next) and to keep its reported state accurate:
    - the lane is in the working state, not held, not closing;
    - its latest report state is "running" (an obstacle, question, stopped or finished report is never prompted);
    - last heard is at least the prompt interval ago (default 15 minutes), and no prompt went out after it;
    - OMP reports the lane session idle, with the recorded session ID, no pending dialog, goal not suspended;
    - no owner message for that lane is queued or awaiting delivery (the owner's message goes first).
13. The prompt goes only to an idle session; it never interrupts a turn. Outcomes: arrived (non-urgent event),
    delivery uncertain or held (urgent event naming the hold; never resent), refused (one urgent event per silence
    spell). An exception during delivery is reported once per (lane run, reason) with the advice to inspect, not
    resend.
14. Report sending applies only when the lane has a bound messenger thread and sending is enabled. A report is
    sent when: the thread has been quiet (no lane report, no automatic send, nothing passed on by the lead) for at
    least the send interval (default 20 minutes); the report was not already sent or passed on (compared by report
    ID); its kind is in the allowed list (default: all); and, if the policy requires an owner-facing summary, the
    report has one. At most one automatic send per report, and none after the last time the lane was heard.
15. Sent text: a short fixed prefix saying it comes from the lane's last report, then the summary (or the report
    text when raw text is allowed). Before sending, the home directory becomes "~", other absolute user paths become
    a placeholder, and backticks become plain quotes.
16. A failed send stays unsent and is retried at the next evaluation; the failure is reported once per (reason,
    report). A report withheld by policy is reported once per (reason, report) as needing the lead to pass it on.
    A successful send records time, report ID and messenger receipt.
17. Wait ended: when a lane's latest report is an obstacle that names a job or an approval it waits on, and the
    daemon sees that job finish or that approval answered, it emits one urgent event per wait saying the wait is
    over, that the lane's obstacle report still stands, and that the lane must not be continued on this basis alone.
    A failed observation is reported once per reason. Reports older than the lane's opening are ignored.
18. Dialog hold: when OMP reports the lane held by pending dialogs, emit one urgent event listing their kinds and
    IDs; emit again only when the set changes; clear silently when none remain.

### C. Stuck lanes and ceilings (WS-16, WS-17, WS-18, WS-21)

19. Oversight covers every working lane whose report log exists. It never acts on a held, closing, replaced or
    reopened run: each action re-checks that the run (opening time, run ID, session ID) is the one observed.
20. Evidence of progress, newest time wins: a new report, a new step record, or a change of the work fingerprint.
21. Work fingerprint, sampled at most once a minute and only while the lane has been busy for at least half the
    busy threshold: the git status list of the lane's working tree (untracked files listed one by one; paths under
    any node_modules directory and Cataly's own state files excluded) with each listed path's size and modification
    time, plus the same for the lane's declared work files. Limits: 256 paths, 10 s per git call, no shell. Over the
    limit, a git failure or an unreadable declared file makes the sample incomplete; "not a git work tree" is an
    ordinary empty list. Git environment variables that redirect the repository are cleared for the call.
22. Stuck: OMP reports the lane busy on the same turn for at least the busy threshold (default 20 minutes) AND the
    newest evidence is at least the quiet threshold old (default 20 minutes) AND the latest sample is complete AND
    the latest report is not an obstacle, stopped or finished. Then append a stuck activity record to the report log
    and emit one urgent event. An incomplete sample never declares a lane stuck.
23. Recovery: new evidence after a stuck verdict appends a running activity record and emits a non-urgent note.
24. Ceilings: wall-clock minutes since the lane opened, tokens, cost in US dollars. Effective ceiling per kind: the
    lane's own launch value, else the configured per-lane value, else the configured default; none or "off" means no
    ceiling. Values must be finite and non-negative.
25. Ceilings are checked on every evaluation and do not depend on a successful evidence sample. A kind is reached
    when the observed value is at or above its ceiling.
26. On reaching any ceiling: append an obstacle report naming every reached ceiling (skipped if the latest report
    has identical text), emit one urgent event per distinct text, and hold the lane (interrupt the turn and suspend
    its goal, through the lane-lifecycle hold). The lead may raise or remove the ceiling and continue the lane. A
    stopped or finished lane gets no ceiling actions.
27. If a token or cost ceiling is set but that total is unknown (see 30), emit one warning per kind that the
    ceiling cannot be checked. Unknown never counts as "below the ceiling" in summaries or health reports.
28. Oversight failures (sampling, spend reading) are advisory: reported once per distinct reason, cleared silently
    after the next complete evaluation. Report sending and delivery continue regardless.
29. At close, record one final token and cost reading per lane run (lane plus opening time). A retried close reuses
    the recorded final reading.

### D. Spend accounting and account limits (WS-20, WS-22, WS-23)

30. A lane's totals cover only its own interval: from its opening (or a later lower bound) to its close or now.
    Totals are "unknown", never zero, whenever any issue below occurs. Tokens are the sum of input, output and
    cache-write tokens; cache-read tokens do not count. Cost is the per-call cost OMP recorded, which is an estimate
    at API list prices and not an invoice. Every view says both.
31. Sources are OMP session files only, in OMP's sessions directory for the lane's working directory. Start from the
    lane's current and earlier session IDs; follow each session's continuation successor while exactly one exists;
    a missing, duplicated or cyclic link is an issue.
32. A billed call is an assistant message carrying usage, or a separate model-usage record, identified by its record
    ID and time. The same call seen twice with different numbers is an issue.
33. Subagent and advisor sessions count when their header names this session (or an earlier ID) as parent, or the
    parent's task result lists them. A child expected from a task result but missing is an issue unless the result
    proves it made no calls. A forked session's copied history is not charged again. Lanes whose session sets
    overlap in time are an issue for both.
34. File safety: only regular files owned by the user with one link; header read limited to 64 KiB; a file over
    64 MiB, a directory over 4,096 entries, or a file that changes while read is an issue. The daemon keeps a
    numeric-only cache keyed by file identity (device, inode, size, change times); content is never cached.
35. Session files are read incrementally (from the last offset while the file identity is unchanged; whole file
    again if it shrank or was replaced).
36. The spend report takes an optional lane, an optional lower bound in hours or days (default the last 24 hours,
    or the whole lane when one is named) and a grouping (lane, thread, or day by call time). It lists per-lane main,
    subagent and total figures, child session count, interval and every issue, as text or JSON.
37. Account limits view: run OMP's usage command with JSON output, 15 s timeout, output up to 1 MiB. Per account:
    provider (known names only, else "unknown"), an anonymous ordinal instead of the account name, state (reported,
    unavailable, disabled), each window's used share, reset time and status, and a note when the five-hour or weekly
    window is missing. Optionally show headroom against a configured warning percentage.
38. The last good answer is cached for the cache period (default 5 minutes, minimum 1); an option bypasses it. The
    view is marked stale when a fetch failed or data is older than the period or dated in the future. With no
    account data it says so plainly and never shows zero use. A failed fetch exits with status 2.
39. The view never retries, never switches account, provider or model, never resets anything, never spends credits.
    The configured OMP command may not contain an action word of its own.

### E. Pull-request follower (WS-24, WS-25)

40. Following takes "owner/repo#N" or a GitHub pull-request URL, plus optional lane and thread (default the lane's
    thread; "none" clears it). Owner and repo names are case-insensitive for identity. Following again replaces
    lane and thread and keeps the original start; stopping ends it; a later follow starts afresh.
41. Optional automatic following (off by default): pull-request URLs in a lane's completion evidence or step
    receipts are followed for that lane. Recording the same lane and thread again changes nothing. A failure never
    blocks the claim or step.
42. Each pass reads every due PR through the GitHub REST API via `gh`. All reads for one PR finish before any state
    changes; a failure leaves that PR untouched and emits one warning per distinct reason until it reads cleanly.
43. Own identity: a configured login, else the authenticated `gh` user, read once. Without it the whole pass waits
    (one warning): own activity must never be reported as someone else's. Configured bot logins and the configured
    commit e-mail addresses also count as own.
44. The first read records a starting point (head, commits, every feedback item and failed check present) and emits
    one non-urgent event. Nothing already there is reported.
45. Later passes emit urgent events for: new feedback from someone else (issue comment, review comment, or review
    except drafts and empty plain-comment reviews; a review whose state changes is new); a newly failed check run
    (failure, timed out, cancelled, action required, startup failure) or commit status (failure, error) on the
    current head, once per check; a head change bringing someone else's commits or no new commits (authors, count,
    old and new short head, whether history was rewritten); merge (with merge commit), close unmerged, reopen.
46. Lists are read 100 rows per page, at most 30 pages; more than 3,000 rows is an error for that PR. The commit
    list is read only when the head moved. If the head is missing from GitHub's 250-commit list, compare from the
    known head; a 404 there means rewritten history with no nameable commits. A head that moves during the read
    causes one immediate re-read, then an error.
47. Once a PR is merged, it is never fetched again. A PR closed unmerged gets one metadata read per pass and a full read only if it
    reopened or merged, or was followed again after it closed.
48. GitHub text is treated as untrusted: flattened to one line, control and bidirectional characters removed,
    excerpts cut to 160 characters, links shown only when https without spaces. Commit e-mails are never shown.
49. The cursor is saved after the events enter the stream, so after a crash an event may arrive twice but is never
    missing.

### F. Timed and condition jobs (WS-26 to WS-30, WS-32)

50. A job has: a unique name (1 to 128 letters, digits, dot, at sign, underscore or hyphen, starting with a letter
    or digit), exactly one trigger, an argument vector (no shell), an absolute working directory, a destination, an
    optional time limit (1 to 86,400 s), and an on-hold flag.
51. The owner (local command line) and the lead (extension tool) may manage jobs. Lanes may not create, change or
    remove jobs. Commands run as the daemon's user.
52. Cron trigger: five fields plus a required IANA time zone; "?" is not supported; day-of-month and day-of-week
    combine with OR. Across daylight-saving changes a local time that does not exist does not fire and a repeated
    local time fires once. Invalid cron or zone is refused before anything is saved.
53. One-shot trigger: an ISO instant with seconds and an explicit offset or Z (never the local zone), in the future
    at creation; a warning is given when it is under two minutes away.
54. Condition trigger: an interval of 30 to 86,400 s, a firing rule (on a change to success, or on every success),
    and an optional "skip identical output" switch. Default time limit 30 s. The first probe runs at creation.
55. A preview option validates and shows the next five trigger instants (UTC and the job's zone) without saving or
    running anything.
56. Cron due handling: an occurrence starting less than 60 s late runs; 60 s or more late is counted as skipped (at
    most 1,000 counted per evaluation) and never replayed. An occurrence due while the previous run or its delivery
    is unfinished is counted as skipped: runs never overlap.
57. One-shot due handling: the job is consumed (put on hold) atomically before running or skipping; 60 s or more
    late is skipped. A consumed one-shot cannot be resumed; a new job is needed.
58. Condition due handling: next probe at the later of finish time and scheduled time, plus the interval. A
    backwards clock step never probes early.
59. Run: before spawning, durably record a run ID, scheduled instant and start time; spawn with stdin closed and
    stdout and stderr to a new private file per run (condition jobs: stderr in a separate file); record the child
    PID. A spawn failure is the run's error.
60. Time limit: on expiry send TERM to the child, then KILL 5 s later. The limit applies to the direct child process
    only; anything it detached keeps running, and the job view says so.
61. Condition outcome: success (exit 0), failure (non-zero), error (time limit, signal, spawn error, or the output
    could not be hashed). Error: the error streak grows, its first error emits one warning, priming is unchanged.
    Failure: the job becomes primed. Success: fire if the rule is "every success" or the job was primed, then
    unprime; with "skip identical output", do not fire when the SHA-256 of the exact output equals the last fired
    output. A new job starts primed, so its first success fires. The first non-error after an error streak emits
    one recovery note.
62. Condition state is saved with the run record before any delivery. Output of a success that did not fire is
    deleted. The output of the latest failing or error run is kept for diagnosis and the previous one deleted.
    Warnings name the run and its files, never their contents.
63. Destinations: keep locally (nothing leaves the machine); give to the lead (urgent event with a header: job,
    scheduled time, exit code, time-limit flag, signal, byte count, log path; then the output if at most 64 KiB,
    else the path only); send to Discord (channel or thread ID), Telegram (chat ID, optional topic ID) or Slack
    (workspace, account and channel, optional thread timestamp; the signed-in account must match).
64. Messenger delivery is one attempt with a 30 s deadline, cancelled by daemon shutdown. Text over 2,000
    characters becomes the header plus a pointer to the log. HTTP 429 or 5xx, a missing or malformed
    acknowledgement, or cancellation means "uncertain": recorded, reported once to the lead, never retried.
    Mentions are suppressed. Message IDs are kept as receipts.
65. Delivery is one of: waiting, kept locally, given to the lead, sent, uncertain. A run is in progress, finished
    or broken off.
66. Hold, resume, remove: resume is refused while a run is in progress; after a broken-off run it needs an explicit
    acknowledgement and never retries that occurrence; resuming a cron job recomputes its next occurrence from now;
    resuming a condition job schedules now plus the interval. Remove is refused while a run or its delivery is
    unfinished, and never deletes run logs.
67. Daemon start: a run recorded as in progress becomes broken off (delivery uncertain) and its job goes on hold; a
    waiting delivery becomes uncertain. Neither is retried, and the recorded PID is never signalled.
68. Daemon stop: TERM to running job children, KILL after 5 s, record their real end; a condition outcome not yet
    applied is recorded as broken off and its job put on hold.
69. Job view: trigger, state (waiting for trigger, on hold, running, delivering, delivery uncertain, timed out,
    succeeded, failed, skipped, broken off), next time, last run with output paths, skipped count, error streak.
70. The job store is written atomically (temporary file, fsync, rename, directory fsync on Unix). A store failing
    validation is never rewritten or run; the daemon reports it and runs no jobs until repaired.

### G. Stream jobs and the lead's tool (WS-02, WS-04)

71. A stream job runs a long-lived command (argument vector, no shell, the lead's working directory) and turns each
    output line matching its regular expression into an event (urgent unless the job is marked low priority).
72. A line over 64 KiB is not filtered; it is written to a private file and one event gives its path and size.
73. Restart policy: none (exit reported once), or up to N restarts (0 to 100, default 5) with exponential backoff
    from a given delay (default 10 s). A clean exit with "run until success" ends the job. Exhaustion stops the job,
    reported once. An optional silence alert emits one event per quiet spell of at least M minutes; any output
    re-arms it; the child keeps running however long it is quiet.
74. The extension tool offers start (name, command, filter, restart policy, silence alert, priority), stop, and
    list (state, last exit, restart count, next restart time). Names are unique. The tool exists only in the lead
    session; the extension forwards calls to the daemon and holds no state.
75. Stream jobs belong to the daemon and survive restarts of the lead's OMP process. They end when stopped, when
    their linked lane closes (10), or when the owner removes them.

### H. Start-up summary and health report (WS-33, WS-34, WS-40)

76. After the daemon starts (once lane reconciliation, LANE-27, is done) and on demand, it produces a read-only
    summary for the lead: runtime availability; lead session state; per lane its status and latest report kind,
    time and state, or why the report log is missing or damaged; held and suspended lanes; drift from LANE-27; open
    requests and decisions (evidence-reporting); one line per timed job and stream job; messenger sign-in checks
    (Slack auth test, Discord and Telegram identity calls), each with a 10 s deadline and never showing provider
    error text; the number of health warnings. It ends "complete" or "incomplete" (any unreadable input, drift or
    failed check). Producing it changes nothing.
77. Health report checks, each OK, INFO or WARN with an optional next step: configuration valid; OMP executable found
    and its version supports the public surfaces Cataly needs; lead session live with its observed model; each open
    lane's requested versus observed model; lane records and report logs readable; active lanes with no ceiling at
    all (WARN); repositories in completion conditions without a configured commit identity (WARN); damaged request or
    lesson logs; each messenger credential present or absent (never its value); each built-in job's last good tick
    within twice its cadence; job store valid.
78. The owner may list check names to ignore in configuration. Output is text or JSON; exit status 1 when any
    non-ignored WARN remains.

### I. Configuration (WS-35)

79. One configuration file in the Cataly home directory. The environment may override only the Cataly home, the
    configuration file path and the state directory. Built-in defaults apply beneath the file.
80. Validation is strict: unknown keys, wrong types, negative or non-finite numbers and out-of-range values are
    refused with the exact setting path. Ceilings accept a number or "off".
81. Settings in this area, with defaults: progress prompt after 15 min; send after 20 min; sending on, allowed
    report kinds (all), raw text allowed (yes); busy threshold 20 min; quiet threshold 20 min; default ceilings
    (none); per-lane ceilings; declared work files; git sampling on; account limits view (off), cache 5 min, warning
    percentage (none); automatic PR following (off); own GitHub login and bot logins; commit identities per
    repository; health checks to ignore; backlog limits (6). Host-pressure thresholds belong to LANE-08 and LANE-09.
82. The daemon reloads configuration on request. An invalid new file is refused, the running settings are kept and
    the error is reported once. New cadences and thresholds apply from the next evaluation.

## Third-party interfaces used

- OMP: extension API (register a tool; send a message that triggers a turn or waits for the next turn); rpc-ui
  events for busy/idle, busy-since, pending dialogs and goal state; session files (header with ID, parent and
  continuation, assistant messages with usage, model-usage records, task results); the usage command with JSON
  output; version output.
- GitHub REST API via `gh api`: pulls, pull commits, compare, issue comments, review comments, reviews, check runs,
  combined status, authenticated user.
- git: status in porcelain form with NUL separators; rev-parse of the top level.
- Discord, Telegram and Slack HTTP APIs for job deliveries and sign-in checks (Slack auth.test).
- Cron expression semantics and the IANA time-zone database; POSIX signals, fsync and atomic rename.

## State that must survive a restart

- Job table: every timed, one-shot, condition and stream job with its settings, hold flag, next time, skipped
  count, last run (ID, scheduled and start times, PID, end, exit, signal, time-limit flag, delivery state,
  receipts), condition state (primed, last outcome, last fired output hash and run, error streak, kept failure
  run), stream job restart count. Run output files.
- Per lane: last prompt and last prompt failure; last automatic send (time, report ID, receipt); last pass-on by
  the lead; last notice per kind and per wait; current dialog set; last oversight error; stuck state, last
  evidence time, last fingerprint, busy-since; ceiling notices sent; final spend reading per run.
- Followed PRs (repo, number, lane, thread, start, last follow time) and per-PR cursor (head, commit list, seen
  feedback and check keys, state, close time, last read).
- Transition memory of every watcher, including host pressure state and per-subject warnings.
- Event backlog and the lead's delivery position.
- Disposable: the spend number cache and the account limits cache.

## Problems in the old approach, and better ideas

- Six long-lived watcher commands (lane loop every 30 s, silence loop, dead-session loop, host loop, PR loop, a job
  runner polling every second) plus a background-command manager inside the lead's OMP process, all printing tagged
  text the lead filtered with hand-written regular expressions. Better: daemon jobs, typed events, one routing rule.
- Because filters could silently drop events, a health check fed synthetic sample lines through every recorded
  filter, and a list of routine line patterns decided what started a turn. Better: kind and urgency are fields.
- The extension rewrote a health file every second so the health check could prove, through process start times
  and ancestry, that watcher children were alive. Better: the daemon knows its own jobs.
- About 770 lines parsed shell command lines (quoting, substitutions, env and sudo options, launchers) to guess
  which watcher belonged to which finished lane. Better: an explicit lane link on each job.
- Background commands were restored by replaying entries in the OMP session plus a copy on disk, with three modes
  and two restart schemes. Better: the daemon persists jobs; one restart policy.
- Stuck detection parsed an elapsed-time label from the terminal, and report lines were scraped from screens with a
  two-sample hold and template filters. Better: OMP rpc busy-since and the report tool only.
- Evidence sampling used per-lane writer locks named by hashed run identity, a file window persisted before
  hashing, submodule special cases and a second sample to absorb side-effect changes. Better: one bounded
  fingerprint per minute inside the single writer.
- Ceilings only wrote an obstacle report; a lane could keep spending. Better: hold the lane (fail closed).
- Spend could come from an external command or from session files, and another external command recorded costs.
  Better: session files only, read incrementally.
- Thread sends went through a configurable command template. Better: native messenger clients.
- The job runner was its own process, guarded by an ownership marker and a file writer lock retried every 10 ms.
  Better: the daemon is the only runner and sleeps until the next due time.
- PR warnings and host-pressure state lived in memory, so a restart repeated them. Better: persisted transitions.
- systemd timers could be adopted for display, Linux only, while macOS is first class. Better: native jobs.

## Questions for the owner

1. Should reaching a time, token or cost ceiling hold the lane automatically (this spec's default), or stay
   advisory as before?
2. Should lane reports still go to messenger threads automatically after 20 quiet minutes, or should the lead always
   pass them on? Which report kinds may go out without the lead?
3. Where do your personal watch scripts live and what do they check? This spec assumes condition jobs and stream
   jobs cover them all; any needing more (state carried between probes, several destinations) should be named now.
4. Pull-request events go to the lead today. Should they also go straight to the lane's thread?
5. Should account limits raise an urgent notice when a window crosses the warning percentage, or stay display-only?
6. After a daemon crash a broken-off job stays on hold until acknowledged. Keep that for cron jobs, or let them
   continue at the next occurrence?
