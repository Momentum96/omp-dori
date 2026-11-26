# Lane lifecycle: behaviour spec

Behaviour spec used to write Cataly 0.2; see docs/provenance.md.

## Purpose

The lead (the owner's main OMP session) splits work into lanes. A lane is one independent OMP session that does
one job with its own brief, working directory, model and completion condition, and reports back to the lead and
to one messenger thread. This area covers a lane's whole life: opening it (inputs, host capacity, model check,
start confirmation), the durable lane record, the binding to a messenger thread, holding and continuing it,
long-term suspension, the completion claim and the close that follows, reopening, noticing a lane whose process
died, and repairing state after a crash. It also covers the shared limit on heavy jobs and the host memory checks.

Users: the lead session (opens, holds, closes, reopens lanes), the owner (authorises closing and cleanup, grants
exceptions), the lane session itself (reports, claims completion), and the daemon (enforces every rule below).
Message delivery to a lane, report storage, supervision ceilings and the completion-signal checker are specified
in their own areas; this spec only states what lifecycle needs from them.

## Capabilities

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| LANE-01 | Open a lane from validated inputs | lead | KEEP | The core action; inputs and refusals are simple to test. |
| LANE-02 | Assemble the brief the lane receives (owner text plus lane instructions) | launch | SIMPLIFY | Keep a short appended instruction block; stop rewriting the owner's brief file in place. |
| LANE-03 | Brief templates with variables and includes | operator | DROP | The lead writes briefs; reusable text can live in OMP's own prompt templates. |
| LANE-04 | Preview a launch without side effects | lead | MERGE into LANE-01 | A flag on open, not a separate path. |
| LANE-05 | Pin the model and check it before the first prompt | launch | KEEP | Prevents silent cost or quality changes; one exact comparison. |
| LANE-06 | Confirm that the lane really started and has its goal | launch | SIMPLIFY | Use OMP's rpc answers and events; drop screen reading and failure-phrase lists. |
| LANE-07 | Origin message, per-start run identity, idempotent launch by request ID | launch, reports | SIMPLIFY | Keep two plain identifiers and a dedupe table; drop the replayed event journal. |
| LANE-08 | Admission guard: memory, container headroom, disk, session count | launch, reopen, queue | SIMPLIFY | Keep the checks, apply them to every start, and treat unknown as "hold". |
| LANE-09 | Host pressure notices (alert and clear) | lead | SIMPLIFY | One daemon sampler that reports only on change. |
| LANE-10 | Heavy-work slots for builds and test runs | lanes | SIMPLIFY | A fair in-daemon semaphore instead of lock directories polled every 15 s. |
| LANE-11 | Launch after predecessor lanes (queued launch) | lead | SIMPLIFY | Keep the dependency rule and manual start; drop five lock layers. |
| LANE-12 | Adopt an already running terminal session as a lane | lead | DROP | A second way in that has no trusted identity; launch covers the need. |
| LANE-13 | Durable lane records, status history, listing | everyone | SIMPLIFY | One writer (the daemon) replaces per-file locks and stale-lock takeover. |
| LANE-14 | Session identity and when it may be trusted | every action | SIMPLIFY | The daemon owns the child process; drop process-table and start-time guessing. |
| LANE-15 | Knowing where the lead is | launch, reports | SIMPLIFY | The daemon started the lead, so there is one source, not two that can disagree. |
| LANE-16 | Messenger thread binding and the thread-finished hook | lead, messengers | KEEP | Owner-visible behaviour; one policy instead of two. |
| LANE-17 | Hold and continue a lane (pause/resume) | lead | KEEP | Needed to stop work cleanly without losing the session. |
| LANE-18 | Notice that a held lane is active again | lead | SIMPLIFY | Event-driven, one notice per active stretch, never an automatic continue. |
| LANE-19 | Long-term suspension and giving up (park/abandon) | lead | KEEP | Frees a process while keeping files; abandon is the same flow with another outcome. |
| LANE-20 | List held and suspended lanes with reasons | lead, start-up summary | MERGE into LANE-13 | A filter on the listing. |
| LANE-21 | Completion claim, objection window, automatic close | lanes, lead | KEEP | The safety rule that lets lanes finish without the lead babysitting. |
| LANE-22 | Close by the lead | lead | SIMPLIFY | Same checks as the automatic close; the old manual path skipped one. |
| LANE-23 | Safe close: quiescence, recorded intent, process exit, receipt | close, park | SIMPLIFY | Keep intent-before-action; process ownership removes most proof work. |
| LANE-24 | Lane worktrees: discovery, unsent-work check, removal | close | KEEP | Protects unpushed work; record worktrees explicitly instead of guessing. |
| LANE-25 | Reopen a closed or suspended lane on its exact saved session | lead | KEEP | Continues work with full context; strict identity checks are cheap. |
| LANE-26 | Detect a lane whose process died | daemon | SIMPLIFY | Child exit and extension disconnect are immediate; drop screen markers. |
| LANE-27 | Reconcile records with reality after a crash or restart; drift report | daemon, lead | SIMPLIFY | One pass at daemon start plus an on-demand report. |
| LANE-28 | Same-host control of any OMP session (read, follow, send, interrupt, model, close, open) | operator | MERGE into LANE-13 | Useful verbs become lane controls; control of unregistered sessions goes. |
| LANE-29 | Wake an unregistered saved session, with its own on/off policy file | operator | DROP | Bypasses the lane record; reopen covers registered lanes. |
| LANE-30 | Per-lane shell command rules with one-time exceptions | owner, lanes | SIMPLIFY | Keep fail-closed rules and owner-only grants; one snapshot, no journal replay. |
| LANE-31 | Observe a lane and answer its pending dialog | lead | KEEP | Small, bound to the current session; needed when a lane asks a question. |
| LANE-32 | Support for non-OMP agents (screen goal markers, argv/env session discovery) | launch, close | DROP | OMP is the engine; this doubles every path. |
| LANE-33 | Execution journal replayed to prove which run wrote a report | reports | MERGE into LANE-07 | Two fields on the lane record carry the same facts. |

## Requirements

### Opening a lane (LANE-01, LANE-02, LANE-04, LANE-05, LANE-06, LANE-07)

1. Only the lead session or the owner may open a lane. A lane session may not open, close or reopen other lanes.
2. Inputs: key, title, brief (a file path or inline text), completion condition, and optionally messenger thread,
   working directory (default: configured default directory), model (default: configured default model), origin
   message ID, a new git worktree to create (path, branch, base), runtime (headless default, or tern), supervision
   ceilings (wall minutes, tokens, cost; empty means the configured default, an explicit "off" disables one), and
   extra paths whose changes count as activity.
3. Key rule: 2 to 61 characters, lowercase letters, digits, dot, underscore and hyphen, starting with a letter or
   digit. The word "lead" is reserved. A key already used by any lane record (open or closed) or by a live queued
   launch is refused.
4. The completion condition must parse as a list of machine-checkable signals (see the completion-signal area).
   An unparseable or empty condition is refused at open time, never discovered at close time.
5. The origin message ID, when given, must have the exact form of a Discord, Telegram or Slack message reference.
   Anything else is refused. It is metadata: it never proves delivery or completion.
6. Ceilings must be finite and non-negative; anything else is refused.
7. The working directory must exist and is stored in canonical form (symlinks resolved).
8. A preview option runs every validation and prints the assembled brief and warnings, then stops. It writes no
   file, creates no record and starts no process.
9. Opens of the same key are serialised; a second open of the same key while one is in progress is refused at
   once with "busy", not queued.
10. Request-ID dedupe: an open may carry a caller request ID. The daemon stores a digest of the inputs with it.
    The same ID with the same inputs returns the earlier result; with different inputs it is refused; if the earlier
    attempt is still unconfirmed it is refused with "inspect first". Stored dedupe entries keep only the digest and
    outcome, never the brief text.
11. Every start (first open and each reopen) gets a fresh random run ID, stored on the lane record and passed to
    the lane process, so reports can be tied to the run that wrote them.
12. Brief assembly: the daemon builds the text the lane receives as owner brief, then a fixed lane instruction
    block, plus standing rules supplied by the lessons area. It stores this assembled text in its own state and
    never writes to the owner's brief file. A brief file that is missing or unreadable refuses the open.
13. The instruction block tells the lane: its key, thread and completion condition; where its brief is; that the
    lead owns the thread's title and finished state; to keep to the recorded repository, branch and acceptance; to
    ask the lead about missing or conflicting facts instead of inventing them; never to redeem coupons, spend or buy
    credits, enable overage or switch account, provider or model, and to stop and report when a quota is reached;
    how to report and claim completion through the extension's tools; its ceilings; and the git identity to use
    per repository when one is configured. It must not contain secrets or channel/user IDs beyond the thread
    reference.
14. Model rule: the model must be one exact provider/model pair, optionally with one of OMP's thinking-level
    suffixes. Aliases, lists and wildcards are refused before any process starts.
15. Start sequence for a headless lane, all within one start deadline (default 180 s, configurable):
    a. check admission (LANE-08); on hold, refuse with the reasons and change nothing;
    b. create the git worktree if requested (refuse if the path exists) and record it as owned by the lane;
    c. write the lane record in a "starting" state before spawning anything;
    d. spawn OMP in rpc mode in the working directory with the requested model, marked as a lane with its key and
       run ID;
    e. wait for OMP to report ready with a session ID and session file, the expected working directory, no open
       dialog, and a model readback;
    f. compare the model readback with the request (provider and ID exact, thinking level equal when one was
       requested). On mismatch or no readback: do not send the brief, keep the process for inspection, mark the lane
       "start held: model", and tell the lead. Never substitute another model;
    g. record session ID and file on the lane, then send the kickoff (read the assembled brief, act as this lane)
       and set the goal through OMP's goal feature;
    h. confirm the goal reads back active (or already complete) and the session ID, file and process are unchanged.
16. Start outcomes, reported to the caller in one line: started; finished already (the lane closed during the
    wait); failed (nothing was sent to the agent: the process is stopped, any worktree created in step b is
    removed, and the record is deleted); unconfirmed (the kickoff may have reached the agent: the record and process
    are kept, the lane is marked for inspection, and nothing is retried automatically).
17. Tern lanes follow the same steps, except that ready, session identity, model readback and goal state come from
    the Cataly OMP extension inside the tern block reporting to the daemon. Typing into a terminal is used only for
    the initial command line. Screen text is never evidence of anything.

### Host capacity and heavy work (LANE-08, LANE-09, LANE-10)

18. Admission is checked for every open, queued start and reopen. Any one failing check holds the start:
    - free memory below the minimum share (default 20 %). On Linux, free means available over total memory; on
      macOS, the kernel's memory pressure level;
    - on Linux, container memory headroom below the same minimum (see 19);
    - free disk on the working directory's filesystem below the minimum (default 50 GB);
    - number of live lane sessions at or above the maximum (default 20; 0 disables the check);
    - any of these values unknown or unreadable. Unknown always holds; it never counts as "plenty".
19. Container headroom (Linux): find the process's memory cgroup (v2, or v1 when the memory controller is still
    there), walk from it up to the highest visible ancestor, and at each level with a limit compute headroom as
    limit minus usage plus clean reclaimable page cache, as a share of the limit. Reclaimable cache is file cache
    minus shared, mapped, dirty and writeback pages, never below zero or above usage. Check the hard limit and the
    soft (high) limit separately and use the smallest headroom of each. "max" and v1's huge sentinel value mean no
    limit. A path containing ".." or an unreadable control file makes the sample unknown.
20. Pressure notices: the daemon samples load, memory, headroom, disk and session count every minute. It sends the
    lead one notice when the host enters alert (load above the alert level, default 150, or any admission check
    failing, or sessions above the maximum) and one when it clears, and separately one when load drops below the
    "ready for heavy compute" level (default 80) and one when it rises again. No notice repeats while the state is
    unchanged.
21. Heavy-work slots: a lane (or any local command) may wrap a heavy command in a request for a slot. The daemon
    grants slots first-come first-served, only while there are free slots (default 1) and the one-minute load is
    below the configured level (default 80). Waiters are re-evaluated when a slot frees or every 15 s. A slot is
    released when the wrapped command exits or the requesting process dies. The wrapped command's exit status is
    passed through. A waiter may give a timeout; without one it waits until cancelled.

### Queued launches (LANE-11)

22. An open may name predecessor lanes. It is then queued: validated and its brief assembled now, stored with the
    canonical working directory, and the key reserved. At least one predecessor is required, each distinct, not the
    key itself, and each must be an existing lane or queued launch. A dependency cycle is refused.
23. A queued launch expires after a positive number of hours (default 24). An expired entry can only be cancelled.
24. Readiness: every predecessor is closed with a completed outcome whose receipt shows every completion signal
    passed. Held, parked, abandoned or merely claimed lanes do not count.
25. The daemon tells the lead when a queued launch becomes ready or newly held, once per change. It never starts a
    queued launch by itself; the lead or owner must start it.
26. Start re-checks: entry still pending and not expired, predecessors still ready, their completion signals read
    back live now, the predecessors' records unchanged during the checks, and admission. The entry is marked
    "starting" durably before the process spawns; after the start it is marked "started". A crash between the two
    leaves "starting", which is shown to the lead and never retried automatically.
27. Cancelling a queued launch is refused once a lane record exists for the key. Cancelling removes no process or
    worktree.

### The lane record and listing (LANE-13, LANE-20, LANE-28, LANE-31, LANE-33)

28. The daemon is the only writer of lane records. Every write is atomic (no reader ever sees half a record).
    Records survive daemon and host restarts.
29. States: starting, running, held, claimed, rejected (claim refused; behaves like running), closing, closed.
    A closed lane carries an outcome: completed, parked or abandoned.
30. Every state change appends a history entry (time, new state, note). The pair (open time, history length) is the
    lane's revision. Every lifecycle action notes the revision it observed and commits only if the revision is
    unchanged; otherwise it reports "the lane moved on" and records nothing. Bookkeeping fields do not change the
    revision.
31. Actions on one lane are serialised in the daemon. An action that cannot get its turn within 30 s fails with
    "busy" and names what holds the lane.
32. Listing shows key, title, thread, state, outcome, opened and closed times, filterable by thread and by open/closed.
    A held or parked filter adds the reason, the free-text resume condition (default "explicit instruction") and
    whether continuing needs resume or reopen.
33. Lane controls for the lead (merging the old same-host control surface): read the last N transcript entries
    (default 20, at most 40); follow events from a cursor, with an explicit gap marker when events were lost; send a
    message with a caller ID, either queued for the next turn or as steering of the current turn, and look up its
    receipt by ID (a lost reply is never answered by sending again); interrupt the current turn; switch model.
    Every control names the revision it saw and is refused if the lane's session changed since.
34. Observe and answer: the lead may read a lane's live state and pending dialog and answer it with one of: cancel,
    yes/no, a text value, or per-question selected options with optional free text. Answers over 64 KiB are refused.
    The answer is refused if, after reading it, the lane is closed, closing, or bound to a different session.

### Session identity (LANE-14, LANE-15)

35. A lane's identity is: runtime (headless or tern), runtime address, OMP session ID, OMP session file, process ID
    plus process start time (and boot ID), and run ID. Earlier session IDs of the same lane are kept in a list.
36. Only the live OMP process the daemon spawned (headless) or the extension instance that registered from the
    recorded tern block with the recorded process (tern) may vouch for a session ID, goal state, model or idle state.
37. Any action that would send to, interrupt, pause or close a session first checks that the live identity equals
    the record. Unknown or different identity holds the action. A held approval dialog also holds any action that
    would type or send.
38. When a tern lane's OMP switches session (for example a new session in the same block), the extension reports it;
    the daemon records the new session ID and keeps the old one in the list. Headless lanes cannot switch silently.
39. The lead's address is whatever session the daemon started or registered as lead. If none is known, lane
    instructions say so and reports are kept locally until the lead is back.

### Messenger thread binding (LANE-16)

40. A lane has exactly one thread reference (messenger and thread ID) or none. It is set at open and kept across
    hold, park and reopen. Routing of messages between the thread and the lane is specified in the messenger areas.
41. A lane never renames, archives or marks its thread finished; the lead owns that.
42. When a lane closes as completed and no other lane on the same thread is still open (closed lanes of any outcome
    do not count), the daemon runs the configured thread-finished hook once with the thread, the close note and the
    key. Its failure is written to the close receipt and does not reopen the lane. Parked and abandoned closes never
    run it. Closes on one thread are serialised so two last lanes cannot both skip the hook.

### Holding and continuing (LANE-17, LANE-18)

43. Hold is allowed from running and rejected. Holding an already held lane succeeds without action. Any other state
    is refused.
44. Hold sequence: pause the OMP goal first (so no goal continuation starts), confirm the goal reads back paused or
    absent or finished, then record the held state with the previous state, then interrupt a running turn. If the
    goal pause cannot be confirmed, the lane is left as it was and the caller is told "not held".
45. Continue is allowed only from held. It resumes the OMP goal, confirms it reads back active (or absent or
    finished), restores the previous state, and restarts the lane's quiet-time clock so a long hold does not trigger
    an instant nudge.
46. Hold and continue are refused while a close is in progress.
47. A held lane that starts a turn (a queued message, a background result) stays held. The daemon tells the lead once
    per active stretch; the next stretch after it went idle is reported again. Nothing ever continues a held lane
    except an explicit continue or reopen.

### Long-term suspension and giving up (LANE-19)

48. Park needs a non-empty reason and accepts an optional free-text resume condition. Abandon needs a reason. Both
    are allowed from running, rejected and held; a pending completion claim must be settled first.
49. Both wait while messages already accepted for the lane are undelivered, and say so.
50. Park requires the lane's saved session file to be resolvable and to match the recorded session ID and working
    directory, because reopen depends on it. Abandon does not.
51. Sequence: hold the lane (43-44) if it is live; record the intent (outcome, reason, condition) in the record; check
    the session is quiet (goal paused, absent or finished, no running turn); close only this lane's session; confirm
    the process is gone; record closed with outcome parked or abandoned and a receipt: reason, time, "worktrees
    kept", whether the session was live, and any unsent-work warnings.
52. Worktrees and files are never removed. If any step fails after the intent is recorded, the lane stays held with
    the intent, and repeating the same command finishes it without changing the reason or condition. A different
    outcome than the recorded intent is refused.

### Completion claim and close (LANE-21, LANE-22, LANE-23, LANE-24)

53. Only the lane itself (through the extension tool) or the lead may claim completion. A claim needs evidence text
    (report ID and shareable receipts; no local paths). It is allowed from running and rejected, and refused while a
    close is in progress.
54. On a claim the daemon records the claimed state with the evidence, tells the lead once, and tells the lane that it
    will close automatically at a stated UTC time unless the lead objects, and that it must stay idle.
55. The lead may object with one or more reasons. The lane goes to rejected and is told the reasons and to claim again
    after fixing them. An objection after the close intent is recorded is refused.
56. When the objection window (default 5 minutes after the claim) passes with no objection, the daemon settles the
    claim. Before closing it checks, in order: no unsent work in the lane's worktrees (60); every completion signal
    reads back live now; no undelivered accepted messages. Unsent work or a failing signal turns into an automatic
    objection naming the reasons. Undelivered messages or a busy lane postpone the settle; it is retried every 30 s.
57. A lead close runs exactly the same checks, without the window. It may attach a lesson (rule plus evidence) that
    is handed to the lessons area only after a successful close.
58. Close commit: confirm the session is idle with no open dialog and matches the record, then record the close intent
    (note, check results, identity including process start time) in the lane record. From then on no claim,
    objection or message changes the decision. Then ask OMP to shut the session down, wait up to 30 s for the process
    to exit, then terminate it. The close is final only when the process with that identity is gone (or the boot ID
    changed); an unreadable process table holds it.
59. After the process is gone: run the thread-finished hook (42), ask the supervision area to record cost (failure
    noted, never blocking), remove worktrees (61), and write the receipt: completed, signal results, note, and one
    line per cleanup step. Then report the close to the lead.
60. Unsent work: for each owned worktree (or, if none, the working directory when it is a git checkout), tracked
    uncommitted changes or commits on no remote count. Untracked files do not. Git state that cannot be read counts
    as unsent work. Git commands run with inherited git override variables removed and the C locale.
61. Worktree ownership: worktrees created at open, plus worktrees the lane registers through the extension. (The old
    name-prefix guess is a fallback only for lanes migrated without a list; see questions.) Removal is the
    non-forced git removal; a failure or a worktree that the artifacts area marks as needed is kept and named in the
    receipt.
62. Crash during close: on restart, a lane with a recorded close intent resumes the close from the step it reached,
    using the recorded intent; it never starts over or re-runs checks with a different result.

### Reopen (LANE-25)

63. Reopen is an explicit lead or owner action only; no timer, condition or message triggers it. It applies to closed
    lanes of any outcome and to a held lane with a recorded park intent.
64. Refusals: no recorded session ID; saved session file missing, or its header names a different session or working
    directory (the header is read from at most the first 1 MiB of the file, skipping OMP's optional title line); the
    old session or any other live session already runs this session ID; admission holds.
65. Sequence: write a new revision in held state (new open time, new run ID, claim, objection, receipt and intent
    cleared, report-tracking fields reset, thread, completion condition and history kept); start OMP resuming the exact
    session file; confirm within the start deadline that the live session ID and file equal the saved ones; then
    continue (45); then, if a note was given, send it once. An unconfirmed note is reported and never resent.
66. If the start or identity check fails, the lane stays registered in held state with the new revision, the reason is
    reported, and the lead must inspect before continuing. Removed worktrees are not recreated.

### Death, crash and reconciliation (LANE-26, LANE-27)

67. Headless: when a lane's OMP process exits or its rpc channel closes while the lane is not closing, the daemon marks
    the lane lost at once (keeping its state underneath) and tells the lead once, with the exit status and the last
    few transcript lines.
68. Tern: the extension keeps a connection to the daemon. If it closes, or tern reports the block gone, the lane is
    marked lost the same way. A connection that misses heartbeats for 45 s is treated as closed.
69. A lost lane is never deleted and its worktrees are kept. Recovery is reopen-style restart of the same session (65),
    on an explicit instruction (see questions for daemon restarts).
70. At daemon start, before taking any new work, reconcile every open lane: live and identical identity, re-attach;
    process gone, mark lost (or finish a recorded close or park intent, 52 and 62); a "starting" record with no
    confirmed kickoff, mark start failed or unconfirmed by the rule in 16. Report a summary line per lane to the lead.
71. On demand the lead can ask for a drift report: lanes whose session is gone, lanes whose live session ID differs
    from the record, tern blocks running OMP that belong to no lane, and lanes with an empty completion condition.
    The report changes nothing.

### Per-lane shell command rules (LANE-30)

72. Configuration may give deny patterns for shell commands and, per lane-key glob (only "*" is special), allow
    patterns. A deny pattern matches anywhere in the command; an allow pattern must match the whole command. Invalid
    patterns fail configuration loading.
73. The rules are snapshotted at open and bound to the lane's session; resume and subagents inherit the same snapshot.
    A missing or unreadable snapshot blocks every shell call of that lane.
74. A blocked call does not run. The lane gets a blocker naming the rule numbers and a digest of the command; the
    command text is not stored. The lane cannot grant itself an exception.
75. The owner or lead, from outside the lane, may grant one attempt for one rule of the most recently blocked command,
    with a reason. Each matching rule needs its own grant. A grant is used up by the next attempt of that exact
    command. The lane must retry itself; granting runs nothing.

## Third-party interfaces used

- OMP: rpc-ui protocol over a child's stdio (ready, session ID and file, model readback, prompt, steer, interrupt,
  model switch, dialogs, events, shutdown); the goal feature (set, pause, resume, read back); CLI flags for model,
  thinking level and resuming a session file; session file header format; extension API for the tern path, the
  report and claim tools, the shell tool-call hook and worktree registration.
- Tern: create a block, run the initial command line, list and close blocks.
- git: worktree list, add and remove; status; rev-list against remotes.
- Linux procfs and cgroup files (process cgroup, mount info, memory limit, usage and stat files, available memory,
  load average); macOS sysctl load and memory pressure level; statvfs for disk free.
- Process start time and boot ID from the OS for process identity.

## State that must survive a restart

- Every lane record: inputs, identity (35), state, revision history, claim, objection, close or park intent,
  receipt, thread, owned worktrees, ceilings, run ID, earlier session IDs.
- The assembled brief text per lane.
- Queued launches with their state, predecessors, expiry and last notice sent.
- Request-ID dedupe entries (digest and outcome).
- Shell rule snapshots and their grants (used or not) and the most recent block per lane.
- The last pressure-notice state, so a restart does not repeat a notice.
- Heavy-slot holders need not survive: a restart frees all slots.

## Problems in the old approach, and better ideas

- Many short-lived CLI processes plus separate polling loops (completion watcher every 30 s, dead-session loop,
  pressure loop, queue loop) and a separate supervisor process for headless sessions. Better: one daemon with timers
  and process events.
- Seven kinds of file locks (record, delivery, launch, thread, terminal ownership, queue graph, capacity) polled every
  50 ms with stale-lock takeover. Better: one serialised queue per lane inside the daemon and a thread-level queue
  for closes.
- Startup, pause and death were judged by reading terminal screens: about 25 failure phrases with careful word-edge
  rules, a goal marker on screen, counting "goal paused" lines, and dead markers that had to appear twice an interval
  apart. Better: OMP rpc answers and extension events only.
- Session identity was guessed from the process table, child environment and session files created near the process
  start time. Better: the daemon owns the child, or the extension registers itself.
- Launch rewrote the owner's brief file in place with backup and restore files, and private attempt directories for
  generated briefs. Better: keep the assembled text in daemon state.
- The admission guard was only used by queued starts and an advisory check; direct launches and reopens skipped it.
  Unreadable memory counted as 100 % free. Better: every start, unknown holds.
- The lead's manual close skipped the unsent-work check that the automatic close made. Better: one rule.
- Two thread-finished policies and an extra "verified but waiting" state existed only to wait for a thread lock.
  Better: one policy, serialised closes.
- Every path carried a second branch for non-OMP agents. Better: OMP only.
- A separate same-host control surface could open and wake sessions with no lane record, with its own receipts,
  lock files and policy file. Better: those verbs as lane controls.
- Worktree ownership was guessed from branch or folder names starting with the key. Better: record worktrees.
- Heavy slots had no ordering, so a waiter could starve. Better: first-come first-served in the daemon.
- Idea: on Linux, run each lane in its own transient systemd scope with memory high and max limits, so one lane
  cannot starve the host; on macOS rely on the admission guard only.

## Questions for the owner

1. Headless lanes talk rpc over the daemon's pipes, so a daemon restart ends them. Should the daemon resume every
   lane that was running from its exact session file on start (goal state kept), or leave them lost until the lead
   reopens them?
2. Must tern lanes support the full lifecycle (hold, park, reopen, safe close), or only open, observe and close,
   with headless the only fully managed runtime?
3. Keep per-lane shell command rules, or rely on OMP's own approval gate and OS accounts?
4. Keep queued launches after predecessors? Is it used?
5. Add per-lane memory limits through systemd scopes on Linux?
6. For lanes migrated without a worktree list, may close fall back to the key-prefix guess, or keep all worktrees?
7. Is the 5-minute automatic close after an unopposed claim still wanted, or should the lead always confirm?
