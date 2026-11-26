# Coordination: behaviour spec

## Purpose

Cataly keeps several OMP sessions working for one owner. This area covers the layer that coordinates them over
time: the lead session (the only one that speaks with the owner), the supervisor (a minute-scale watch that reminds
quiet lanes and alerts the lead about stalls), the planner (an hourly pass that starts queued work and asks the
owner's open questions through the lead), per-session mailboxes, named exclusive slots with a priority queue, work
items, the owner notes file, the review verdict and merge check, event commands, wait tracking, text fitting, short
session addresses and safe writing of setup files. Users: the owner (through the lead or
the CLI), the lead, lane sessions and the daemon itself.

This is the "supervision area" the evidence and reporting spec refers to: it reads that spec's last-progress mark
(EVR-08, EVR-16) and defines no progress measure of its own. The mailbox here is the one delivery path; lead-to-lane
delivery (EVR-33) and report wake lines (EVR-17) use it. Today the lead, a second model session, the supervisor and
the planner are four long-lived processes; in the new Cataly they are one Rust daemon (with two timers and a thin CLI
client) plus the lead's OMP session, and every record has one writer.

## Capabilities

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| COORD-01 | Bring up the lead and switch on supervisor and planner with one command | owner | SIMPLIFY | One daemon start; no panes or loop processes to open. |
| COORD-02 | Lead role: sole owner-facing session, receives tagged notices | owner, lead | KEEP | Core of the product's shape. |
| COORD-03 | Always-on second model session that unblocks waiting lanes | supervisor | DROP | Burns tokens all day; stall alerts to the lead cover it. |
| COORD-04 | Supervisor and planner as separate looping processes with lifetime locks | operator | DROP | Become daemon timers; the daemon's own single-instance rule covers them. |
| COORD-05 | Register an existing terminal session as lead or helper by hand | operator | DROP | Second way in without trusted identity; matches LANE-12. |
| COORD-06 | Supervisor: reminders to quiet lanes, one stall alert per progress round | lead | SIMPLIFY | Keep the rules; drive them from per-lane deadlines and OMP events, not a 60 s poll. |
| COORD-07 | Reminder and alert texts that name the wait target and the options | lanes, lead | MERGE into COORD-06 | Content of the same notices. |
| COORD-08 | Death notice when a lane's session vanished | lead | MERGE into LANE-26 | Already detected at once there; mailbox rules below cover the notice. |
| COORD-09 | Stop, resume and status switch for supervisor and planner | owner, lead | KEEP | Needed safety brake; tiny. |
| COORD-10 | Session mailbox: deliver now when idle, queue while busy, batch on idle | all | KEEP | Never interrupts a busy agent; the base of every notice. |
| COORD-11 | Cross-process per-message side files, address locks, merge-on-unlock, closed markers, delivered-ID file | mailbox | DROP | One writer in one daemon makes all of it unnecessary. |
| COORD-12 | Once-only notices by stable identity | supervisor, planner | KEEP | Stops duplicate reminders after a crash or retry. |
| COORD-13 | In-doubt delivery: kept, never resent, lead told once, owner confirms or releases | lead, owner | SIMPLIFY | Keep the safety rule; with rpc the in-doubt case is rare. |
| COORD-14 | Typed-but-not-submitted text in a terminal session | terminal runtime | SIMPLIFY | Only the optional terminal runtime needs it; submit only into the same agent. |
| COORD-15 | Notices kept for the lead while no lead exists or its mailbox is busy | supervisor | SIMPLIFY | The lead mailbox exists independently of the session; no side queue. |
| COORD-16 | Close a retired session's mailbox to new messages | session host | SIMPLIFY | Mailboxes follow logical targets; explicit addresses of retired sessions are refused. |
| COORD-17 | Operator types one message into a bare shell | operator | DROP | Risky, rare, and the terminal itself does it. |
| COORD-18 | Send result contract (delivered, waiting, in doubt) and its exit statuses | CLI, scripts | KEEP | Callers must never resend an in-doubt message. |
| COORD-19 | Planner pass: reconcile items with lanes, start next items, ask questions, report | lead | SIMPLIFY | One timer job inside the daemon; same ordering and limits. |
| COORD-20 | Planner audit (finished lane left open, stale lane, vanished session) | lead | SIMPLIFY | Keep the first two; vanished sessions are reported at once by LANE-26. |
| COORD-21 | Host load and free-memory hold on planner starts | planner | MERGE into LANE-08 | One admission guard for every start. |
| COORD-22 | Planner run records and the latest-run summary | lead, owner | SIMPLIFY | Bounded history in the store; no separate stamp file. |
| COORD-23 | Planner cadence that survives restarts, with forced run | planner | KEEP | Avoids double or skipped hours. |
| COORD-24 | Planner single-instance file lock with link/rename takeover | planner | DROP | The daemon runs one planner by construction. |
| COORD-25 | Work items: add, list, answer question, drop | lead, owner | KEEP | Simple queue of later work. |
| COORD-26 | Import labelled open GitHub issues as work items | lead | KEEP | Cheap feed; read-only on GitHub. |
| COORD-27 | Retire items from GitHub state (PR merged, issue closed) | planner | KEEP | Read-only checks, keeps the queue honest. |
| COORD-28 | Failed-try counting that turns an item into a question | planner | KEEP | Stops a broken item blocking or looping. |
| COORD-29 | Item questions asked through the lead, one reminder after 6 h | owner | KEEP | Only the lead talks to the owner. |
| COORD-30 | Priority 1-9 shared by items, lanes and slot waiters | all | KEEP | One ordering rule everywhere. |
| COORD-31 | Named exclusive slots with a priority-ordered queue around a command | lanes | MERGE into LANE-10 | Same mechanism as heavy-work slots, with a name and priority. |
| COORD-32 | Compatibility with shell lock directories and their holder text | shell scripts | DROP | Compatibility layer; scripts call the CLI instead. |
| COORD-33 | One-line listing of a slot's holder and queue | status | MERGE into EVR-29 | Part of the status view. |
| COORD-34 | Owner notes file, opt-in, pointed to from the lead's prompt | lead | KEEP | Cheap memory of preferences; never authority. |
| COORD-35 | Owner notes check (structure, dates, size, secrets, no lost decisions) | lead | KEEP | Keeps an LLM-edited file sane. |
| COORD-36 | Daily owner-notes refresh request at a configured hour | planner, lead | KEEP | Off by default; one request a day. |
| COORD-37 | Dated snapshots of the owner notes, diff and revert with backup | owner | SIMPLIFY | Keep snapshot and revert; drop the built-in diff. |
| COORD-38 | Review verdict record: fresh reviewer at the exact PR head | lead | KEEP | The evidence the merge check needs. |
| COORD-39 | Merge check: approving verdict at head plus green checks, head-pinned squash merge | lead | KEEP | Small, fail-closed, valuable. |
| COORD-40 | Custom reviewer command from config | owner | SIMPLIFY | Becomes one event command with the same verdict contract. |
| COORD-41 | Event commands on finish, report and death | owner | SIMPLIFY | Keep argv-only commands, run by the daemon with a timeout. |
| COORD-42 | Verified waits (OMP job, OMP approval, CI check) and free-text waits | supervisor, lead | SIMPLIFY | Observe from OMP events instead of polling probes. |
| COORD-43 | Text fitting for terminal output (display width, safe cuts, one-line names) | CLI, notices | SIMPLIFY | Keep the rules that protect readers; one small module. |
| COORD-44 | Progress clock from report evidence | supervisor | MERGE into EVR-08 | Defined once there. |
| COORD-45 | Lane board in the status view | owner, lead | MERGE into EVR-29 | Status view owns layout; additions listed below. |
| COORD-46 | Short session addresses (unique UUID prefix) | CLI | KEEP | Saves typing; refuses guesses. |
| COORD-47 | Target resolution order for messages | CLI | KEEP | Predictable addressing. |
| COORD-48 | Lane key syntax and reserved role names | all | MERGE into LANE-01 | Lane opening owns key rules. |
| COORD-49 | Lane verbs in the coordination CLI (open, report, finish, close, priority) | lead, lanes | MERGE into LANE-01 | Specified by lane lifecycle and evidence areas. |
| COORD-50 | Anonymous opt-in usage counts over OTLP/HTTP | owner | DROP | Owner decision (2026-11-26): Cataly sends no usage counts or telemetry of any kind. |
| COORD-51 | Safe setup-file write: identity snapshot, preview, compare-and-swap, readback | setup | KEEP | Never clobbers a hand edit. |
| COORD-52 | Report whether the saved setup is effective or shadowed by overrides | setup | SIMPLIFY | One "effective value and its source" listing. |
| COORD-53 | CLI argument safety (value flags take the next word verbatim, empty text refused) | CLI | KEEP | Stops relayed text turning into flags. |
| COORD-54 | CLI follows the installed release home when run through a link | CLI | DROP | Release area concern; the daemon knows its home. |

## Requirements

### Roles and the one-daemon shape (COORD-01, COORD-02, COORD-09)

1. One daemon runs per Cataly home. It hosts the lead's OMP session (headless by default, the terminal runtime
   when chosen), all lane sessions, the supervisor timer, the planner timer, the mailboxes and the slot service.
   The CLI talks to it over a local socket owned by the user (mode 0600); every CLI command is one request.
2. Starting coordination (one command) refuses when a lead is already registered for the home. It brings up the
   lead (resume and fresh start are the session host's rules) and enables supervisor and planner. Installing or
   updating Cataly never enables them.
3. The lead is the only session that addresses the owner. Notices reach it as prompts that begin with a short tag
   naming the kind: lane started, lane report, lane finished, lane dead, stall alert, planner summary, item
   question, owner-notes refresh, in-doubt delivery.
4. Supervisor and planner each have a switch: stop (with an optional free-text reason), resume,
   and status (running, or stopped with reason; for the planner also minutes since its last run and that run's
   summary). Lead and owner may use them; lanes may not. The switch state survives restarts. While stopped, the
   supervisor does nothing at all and the planner records nothing; work items stay editable.
5. Authority: lanes may report on their own key, message the lead, and request slots. Only the lead or the owner
   may open or close lanes, answer or drop items, run the merge check, release in-doubt messages and flip switches.
   The supervisor and planner act with the daemon's own authority but only within the limits in 6-13 and 26-35.

### Supervisor (COORD-06, COORD-07)

6. Scope: lanes that are active or waiting. Finished, closed and dead lanes are ignored.
7. Inputs: the lane's last-progress mark (EVR-16), its last activity (latest report of any kind, or the latest
   moment OMP showed its agent working), its agent state from OMP (working, idle, awaiting approval, error, gone),
   its reminder count and whether this round already had a stall alert. A new last-progress mark, or a completion
   claim, starts a new progress round: the reminder count returns to 0 and the alert marker clears.
8. Stall alert: when 40 minutes have passed since the last-progress mark and no alert was sent this round, the
   daemon queues one alert to the lead, whatever the agent is doing and whether or not the lane is waiting. The
   alert gives the lane key, whether it is waiting, time since progress and its clock time, the session address,
   the reminders sent and the number of reports without new evidence this round, the last evidence item (cut from
   the start to 48 columns, with the full text on its own line when cut) and a suggested next step (check, re-brief
   or close; for a waiting lane: check it is alive or reassign). A waiting lane's alert also names the wait target
   and since when.
9. With no lead registered the alert is not sent and not marked; it is tried again at the next deadline. Once
   queued, the round gets no further reminders.
10. Reminders: only while the agent is idle (never working, awaiting approval or in error) and no alert was sent
    this round. Quiet time is measured from the later of the last-progress mark and the last activity. The first
    reminder is due at 10 quiet minutes, the second at 20; there is no third. Each reminder increments the count and
    is logged on the lane.
11. Reminder text for an active lane says how long since evidence, that a report without evidence does not reset
    the clock, that 40 minutes goes to the lead, and the three ways forward (claim completion with evidence, report
    progress with evidence, report a wait with its target), else continue. For a waiting lane it names the full wait
    target and since when, asks the lane to check the target is alive, and offers: it moved, it finished, it is
    stuck. Both texts state that an exhausted provider quota or credit budget means stop and report, and that no
    coupon, credit, top-up, paid overage or account/provider/model switch may be used to get round it.
12. Triggering: the daemon keeps one deadline per lane (next reminder or alert) and re-computes it on every report,
    agent state change, completion claim and switch change, plus a safety sweep every 5 minutes. A due notice is
    queued within 1 s of its deadline. The supervisor never starts, stops or closes sessions.
13. (Removed with COORD-50: the supervisor keeps no usage counters.)

### Mailboxes (COORD-10, COORD-12 to COORD-16, COORD-18)

14. Every logical target has one mailbox: the lead, and each lane key. A message has sender label, time, text, an
    optional stable identity, and a delivery state (pending, typed, in doubt).
15. Sending: the daemon appends the message, then tries to deliver at once if the target's agent can read now
    (idle, or finished its turn). Working, awaiting approval and error mean wait. A terminal session with no agent
    in front never receives text (it would run as a shell command); the sender is told nobody can read it yet.
16. When an agent turn ends (OMP's turn-end event), the daemon delivers that session's pending messages within
    1 s. No periodic flush is needed; the 5-minute sweep retries anything left.
17. Batching: one pending message is delivered as its text. Several are delivered as one prompt that starts with
    how many arrived while the agent was busy, then each numbered, with its sender, in arrival order.
18. On confirmed delivery the batch leaves the mailbox. Messages that arrived during delivery stay for the next
    turn end.
19. Once-only: a message with a stable identity is added to a mailbox only if no message with that identity is
    pending there or was delivered or confirmed there in the last 30 days. Identities used: reminder (lane run,
    round, number), stall alert (lane run, round), death notice (lane run), item question (item, ask or reminder),
    owner-notes refresh (date).
20. In doubt: if the request to OMP may have reached the session but no answer came (connection lost, timeout after
    30 s), every message in the batch becomes in doubt. In-doubt messages are never resent automatically and are
    skipped by later deliveries. The lead is told once, naming the target, the count, each sender and queue time,
    the reason, and how to confirm or release. If the in-doubt batch was itself such a notice, no further notice is
    sent, so this cannot loop.
21. Listing a mailbox shows each entry with a 1-based index, its state, sender, queue time (UTC) and the first 60
    characters of the text in escaped form, with an ellipsis when longer.
22. The lead or owner may confirm an in-doubt entry (it arrived: remove it, record it as delivered) or release it
    (it did not: make it pending, delivered on the next turn end). Only in-doubt entries; others are refused with
    the reason. A wrong index is refused with the entry count.
23. Terminal runtime only: when text was typed but the submit keystroke failed, the entry is typed. Only the submit
    is retried, and only into the same agent (same process group and identity). If another agent or no agent is
    now in front, the typed entries become in doubt (rule 20). Messages queued after a typed batch wait.
24. Mailboxes address logical targets. A lane whose session was replaced (reopen) keeps its mailbox. A message sent
    to an explicit session address that is no longer the current session of any role or lane is refused. A message
    for the lead is always accepted, even with no lead session, and is delivered once a lead exists. The lane-dead
    notice states how many messages were pending for that lane; they stay until the lane is reopened or closed.
25. Send results: delivered now; waiting in the mailbox; in doubt (do not send again; check, then confirm or
    release). The CLI exits 0 for the first two and with a distinct non-zero status for in doubt. An unknown or
    gone target is refused before anything is stored.

### Planner (COORD-19 to COORD-23, COORD-25 to COORD-29)

26. The planner only opens lanes, messages the lead and writes its own records and item records. It never merges,
    pushes, closes a session or edits an open lane's record. Its GitHub calls are reads.
27. Gate, in order: stopped means skip; a run less than the interval (default 1 h) after the last recorded run
    means skip unless forced. A skipped run changes and records nothing.
28. Reconcile, per item: a running item whose lane is closed becomes done; one whose lane is dead or missing goes
    back to open with one more failed try, and at 2 failed tries gets an open question asking the owner whether
    to try again, rewrite the brief or give it up; one whose lane claimed completion with a GitHub PR link among its
    evidence becomes done when that PR is merged (and the run notes that the lane can be closed); an open imported
    item whose source issue is closed is dropped. A GitHub error becomes a finding and the pass continues.
29. Audit (read-only findings): a lane that claimed completion 2 hours or more ago and is still open; an active or
    waiting lane with no progress for 3 hours or more (a waiting lane names its full target and since when).
30. Start: nothing starts without a lead. Free starts = min(max open lanes - open lanes, max starts per run),
    never below 0; defaults 3 and 2. Candidates are open items with no open question whose key has no open lane,
    ordered by priority, then creation time, then id. If the admission guard (LANE-08) refuses, no item starts and
    the reason is recorded.
31. Each attempt uses one start. A failed open adds one start failure; at 2 the item gets an open question that
    quotes the first 120 characters of the error and asks whether to fix the brief or directory or give it up. A
    start that the session host left in an uncertain state gets a question at once and is never retried
    automatically. A lane that opened but whose start notice did not reach the lead still counts as started. An item
    becomes running only if it is still the same item (same creation time), open and without a question. Leftover
    candidates are summarised with their count, the free lane places against the maximum, and the per-run limit.
32. Ask: each open item with a question is sent to the lead once as an item question; one reminder follows after
    6 hours; no more after that. The question line tells the lead how to answer or drop the item.
33. Then the owner-notes refresh step (rule 49) runs.
34. Record: time, open lanes against the maximum, open lanes not started from an item, item counts (open, with a
    question, running), host load and cores and free memory, and the lists of what it reconciled, noticed,
    started, held back and asked. The newest 168 runs are kept. Then one summary goes to the lead (header line with
    the counts, then each non-empty list, then the questions). If that send fails, the failure is added to the
    record. The lead does not reply to a summary that needs nothing.
35. Cadence: the planner runs once when coordination starts (subject to 27), then sleeps until last run plus
    interval when that is in the future (at least 1 s), else a full interval. A forced run applies to one pass. The
    owner may also ask for a run now. A pass that throws is logged and the next pass is a full interval later.

### Work items (COORD-25 to COORD-27, COORD-30)

36. An item has id (lane key rules, LANE-01, not a reserved role name), title (default the id), brief (copied into
    the store when added), working directory (default the caller's), priority 1-9 (default 5), status (open,
    running, done, dropped), optional open question, ask and reminder times, failed tries, start failures,
    optional GitHub source, creation and update times, and a log of what happened.
37. Adding refuses an invalid id, a missing brief file, a priority outside 1-9, and an id whose item is open or
    running. A done or dropped id may be reused.
38. Answering needs non-empty text and an open item; it appends the question and answer to the item's brief,
    clears the question and its ask and reminder times, and resets both try counters.
39. Dropping refuses a running item (close its lane first); otherwise sets dropped with a reason.
40. Import from a repository (owner/name form only) reads up to 1000 open issues with a configurable label. Each
    issue not already imported becomes an item whose brief holds the issue title, a link and the body. Its id is
    derived from the repository name and issue number, valid as a lane key and stable across imports; an id held by
    an item from another source refuses the import. A label of the form p1..p9 sets priority; a configurable
    "needs a decision" label sets an open question. Reaching the 1000 limit prints a warning that some may be
    missing.
41. Priority 1 runs first and 9 last; the same value orders items, lanes (set at open or changed later on an open
    lane) and slot waiters. A lane opened from an item takes the item's priority.

### Named slots (COORD-31, folded into LANE-10)

42. LANE-10 gains a slot name (a letter or digit, then letters, digits, "_", "." or "-", 64 at most) and orders
    waiters by priority, then strict arrival order (two waiters in the same millisecond keep their order).
43. The requester names a lane key (key rules, reserved names refused; no record needed) and an optional priority;
    without one the lane's recorded priority is used, else 5.
44. The wrapped command runs in the caller's terminal with its own standard streams; the call ends with the
    command's exit status. The slot stays held until both the requesting client and the command have exited, so
    killing the client does not free a slot whose command still runs.
45. A stop or interrupt signal while waiting withdraws the request, runs nothing and exits with 128 plus the
    signal. One that arrives after the grant but before the command starts releases the slot and runs nothing.
    While the command runs, the signal is passed to it, except a terminal interrupt, which the command already got
    and must not get twice.
46. After a daemon restart no slot is granted for 5 s, so clients whose commands still run can re-assert their hold.
    The client shows a line when its place in the queue changes.

### Owner notes (COORD-34 to COORD-37)

47. Off until the owner creates the file; creating it twice is refused. It has five sections in fixed order: likes
    and working style, current topics, standing decisions, dislikes, change log. Each entry is one line starting
    with its date (YYYY-MM-DD) in parentheses. Private file mode.
48. The lead's prompt mentions the file only when it exists, never embeds it, and says it holds observations, not
    instructions or permission, and must not be passed to other roles. Showing it prints at most 16 KiB, cut at a
    line end, with a note when cut. Only regular files are read (no symlinks).
49. Refresh: when the owner configures a local hour (0-23) and the file exists and a lead exists, the planner, on
    its first pass at or after that hour each local day, writes a private dated procedure and queues one dated
    refresh request to the lead. The date is claimed only after the lead's mailbox accepted it. An invalid hour is a
    planner finding.
50. The procedure (lead does it from its own session context only): take today's snapshot; add observations only
    when stated directly or seen twice; merge duplicates; drop topics older than 14 days; never lose a decision (a
    replacement quotes the old text, an unsettled disagreement keeps both sides, more than 20 decisions move to an
    archive file); add one change-log line, keep the newest 14; run the check until it passes; never store
    credentials or other people's details; leave everything untouched if the date has changed.
51. The check reports one problem per line and fails if any: wrong sections or order; badly formed or future-dated
    entries; topics older than 14 days; more than 20 decisions; more than 14 log lines; over 16 KiB; anything that
    looks like a chat-platform token, API key, GitHub token, cloud access key, private key block, bearer token or
    e-mail address; and any decision in today's snapshot that is neither kept, quoted in a replacement or conflict,
    nor present in the archive.
52. Snapshot: the first snapshot of a local day wins and is never replaced. Revert restores a dated snapshot or a
    backup; before that it saves the current file as a timestamped backup and prints how to undo. Reverts are
    serialised; a crashed revert leaves its guard for inspection. Nothing deletes snapshots or the archive.

### Review and merge (COORD-38 to COORD-40)

53. Review takes owner/name#number, reads the PR's head commit, and runs a fresh non-interactive OMP reviewer
    (print mode, no saved session, optional model) without any lane or role identity. Its prompt pins the exact head,
    asks for findings with severity and location, forbids file changes, and asks for one final verdict line:
    approve (no high or medium finding) or reject. An approval grants no permission.
54. The last verdict line counts. No verdict or a non-zero exit writes no record and saves the last 200 lines of
    output to a private diagnostic file named in the error. Otherwise the record holds PR, head, verdict, model,
    time and the last 4000 characters of output. The command exits 0 for approve, 1 for reject.
55. Merge check, against the current head: refuse without a record at that head; refuse unless it approves; refuse
    when GitHub reports no checks; refuse naming every check that is neither passed nor skipped; refuse when all
    were skipped. Then squash-merge pinned to that head. A dry run stops before merging. The check is necessary,
    not sufficient: the lead merges only with the owner's authorisation.
56. A configured reviewer command (argv template, rule 57) replaces the built-in reviewer and must print the same
    verdict line; its records name the model as custom.

### Event commands (COORD-41)

57. Config may name an argv template for: lane finished, lane report, lane dead, custom reviewer. Each element may
    hold placeholders for key, text, evidence, PR, repository, number and head; substitution is per element and never
    passes through a shell. A malformed config is an error naming the field.
58. The daemon runs event commands in the background, at most 4 at once, each with a 30 s timeout after which it is
    killed. A spawn failure, timeout or non-zero exit is logged in one line with the first line of its error output.
    An event command never delays or fails the action that triggered it.

### Waits (COORD-42)

59. A lane reporting a wait may name the target. Recognised: an OMP background job in its own session, a pending
    OMP approval in its own session, a CI check of a PR (optionally one check run). Anything else is a free-text
    label, shown as unverified. Control characters are folded to spaces; inner spaces are kept.
60. A job or approval wait is accepted only if the daemon currently sees it running or pending in the lane's current
    session; it is bound to session identity, start time, the job or call identity and the job start time. An
    attempt to bind an observation older than, or ended compared with, the one already held is refused.
61. A CI wait reads the PR head (must be a full commit hash). With a check run, the run must belong to that head and
    be queued, in progress, pending, waiting or requested; completed is refused. The head is read again and a move
    refuses the wait. CI waits are bound once and not polled afterwards.
62. On OMP events the daemon updates job and approval waits: a different session means ended because the session
    changed; an approval no longer pending means resolved; a job no longer running takes its final status, else it
    is marked as no longer seen. An older observation never overwrites a newer one. Ended means the wait no longer
    applies, never that work succeeded; it is logged and shown, and lane state and progress do not change.
63. Reporting the same target again keeps the original "since"; reporting active clears the wait.

### Text fitting and addresses (COORD-43, COORD-46, COORD-47, COORD-45)

64. Width is measured in terminal columns (wide characters count two) and cuts never split a grapheme. End cuts
    end with an ellipsis; paths are cut from the start so the file name stays. A name (link, path, job id, target)
    is never split across lines; when a line had to cut one, the full name follows on its own indented line.
65. Text from lanes shown on one line has control characters, line separators and bidirectional overrides replaced
    by a space. Durations read "45m", "2h10m", "1d3h"; clock times are local "HH:MM". Default width 120.
66. A headless session's UUID may be shown and typed as its shortest prefix of at least 8 characters, not ending in
    a hyphen, that no other known session (any role or lane, any status) shares. A shorter, unknown or ambiguous
    prefix is refused, listing the matches when ambiguous.
67. A message target resolves in this order: a session address (full or short), a role name, an open lane key, a
    terminal session address. Anything else is refused.
68. The status view (EVR-29) also shows: runtime and where that choice came from, lead address, home path, open
    lanes oldest-progress first with wait target and since, closed lanes folded into one line per status, and one
    line per slot with holder, age and queue ("+N more" when it does not fit).

### Usage counts (COORD-50, dropped)

69-71. (Removed.) Cataly sends no usage counts or telemetry of any kind: no install identifier, no collector, no
    opt-in switch. Nothing leaves the machine except what a feature the owner turned on needs to do its job.

### Setup files and CLI (COORD-51 to COORD-53)

72. Before writing a config file, setup records its identity (device, inode, mode, owner, group, size, change
    times) and SHA-256. It refuses a file not owned by the user, not regular, or group/world-writable (and says how
    to fix it), and a parent directory that is not the user's real directory.
73. A preview reports create, keep or update, the before and after hashes, which keys setup manages, and that
    unknown keys are kept. Publishing re-checks the identity just before an atomic replace (or no-clobber create);
    a change since the snapshot is refused and the user's edit kept. After writing, the file is read back; a
    mismatch is an error.
74. Setup then reports, for each managed setting, the effective value's source: this file, an environment
    override, or unavailable (with the reason).
75. CLI flags that take a value always take the next word verbatim, even if it starts with dashes; a value flag at
    the end is refused. Message text comes from a flag or a pipe; an empty or blank message is refused. Help prints
    usage and runs nothing. Refusals print one line and exit with a distinct status.

## Third-party interfaces used

- OMP rpc-ui: prompt submission and its answer, turn start and end events, agent state, session identity, pending
  approvals, background jobs. OMP CLI: print mode, no-session flag, model flag, appended system prompt.
- GitHub through `gh`: PR head commit and state, issue state, issue list by label, PR checks with pass/skip buckets,
  check-run lookup through the REST API, squash merge pinned to a head commit.
- OS: load average and core count; available memory (on macOS the kernel's memory-status level); process groups and
  the terminal's foreground group (to avoid a double interrupt); file ownership and modes.

## State that must survive a restart

- Mailbox entries with state (pending, typed with agent identity, in doubt with reason) and once-only identities
  for 30 days.
- Per lane: reminder count, alert marker and round, last activity, wait binding and latest observation.
- Supervisor and planner switches with reasons; planner run history (168) and last run time.
- Work items with logs, questions, ask and reminder times, tries.
- Owner notes, snapshots, backups, archive, claimed refresh dates, rendered procedures.
- Review verdict records and diagnostics.
- Not kept: slot grants and queues (rule 46 handles the gap).

## Problems in the old approach, and better ideas

- Four long-lived actors per home (lead, an all-day helper model session, two shell loops in panes, each loop with
  its own lock file). Better: one daemon with two timers and no helper session; the lead opens a lane to unblock.
- The supervisor polled every 60 s, so a message to a busy session waited up to a minute after it went idle.
  Better: deliver on OMP's turn-end event and fire per-lane deadlines.
- Every shared file needed cross-process machinery: locks judged by pid liveness and host name, hash-named claim
  files, per-message side files, rename-aside merges, an ever-growing delivered-identity file, per-address locks,
  closed markers, link and rename takeovers, field-by-field record re-checks. Pid reuse could keep dead waiters in
  line. One writer removes all of it.
- Reports and finish notices for the lead were dropped with a warning when no lead was registered, while death
  notices were kept. Better: the lead mailbox always exists.
- The hourly audit repeated what the supervisor already said (vanished sessions, stale lanes).
- Event commands ran fire-and-forget from short CLI processes and were killed 2 s after the command ended.
- Named locks shared directories with shell scripts that could clear a live lock.
- Usage counts went to the upstream project's collector under the upstream product name. Better: none at all.
- CI waits were bound once and then shown with a stale state forever. Idea: let the PR follow-up area update them.

## Questions for the owner

1. May the always-on helper model session go (COORD-03), leaving stall alerts to the lead?
2. Usage counts: answered (2026-11-26): none at all; COORD-50 is dropped.
3. Event commands (COORD-41): still needed now that Discord, Telegram and Slack relays are native, or drop them?
4. Planner defaults (3 open lanes, 2 starts per run, hourly) and GitHub labels for import: keep or change?
5. Keep the owner notes feature and its daily refresh, or drop it?
