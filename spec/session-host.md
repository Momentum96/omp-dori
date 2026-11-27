# Session host: behaviour spec

Behaviour spec used to write Cataly 0.2; see docs/provenance.md.

## Purpose

Cataly runs its work inside OMP sessions: the standing lead and duty sessions, and one session per lane. This
area is the part that starts those sessions, owns their processes, sends them input, watches what they do, answers
their questions, and stops them. Everything else in Cataly (messengers, lanes, schedules) reaches a session only
through this part.

Default mode: OMP runs as a child of the Cataly daemon in its rpc-ui mode, with no terminal ("headless"). Optional
mode: OMP runs visibly inside a Tern terminal block, and the daemon still controls it, but over a connection the
Cataly OMP extension opens to the daemon instead of over the child's stdin and stdout.

Users: the daemon itself (messenger intake, scheduler, lane lifecycle), the owner through the local command line,
the lead session through Cataly commands, and the Cataly OMP extension running inside each session.

## Capabilities

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| HOST-01 | One process owns every OMP session as its child | everything | SIMPLIFY | Old design ran one detached supervisor per session plus separate loop processes; one daemon is lighter and has one source of truth. |
| HOST-02 | Session record written before the child starts | lifecycle, recovery | SIMPLIFY | Keep "record first, then spawn"; drop the per-session key, socket and marker files. |
| HOST-03 | OMP compatibility check | daemon start, health | SIMPLIFY | Replace the exact pin to a patched OMP build with a minimum version plus feature probing. |
| HOST-04 | Building the OMP command line | every start | SIMPLIFY | Structured config instead of a free-form command template parsed for launchers and placeholders. |
| HOST-05 | Fixed launch policy: rpc-ui mode, always-ask approvals, overlay settings | every start | KEEP | Small, public flags; the safety default must not be configurable away per session. |
| HOST-06 | Child environment and secret handling | every start | KEEP | Prevents token leaks into OMP and into argv. |
| HOST-07 | Start handshake and session identity binding | every start | SIMPLIFY | Identity comes from OMP's state answer and the daemon's own spawn; drop the custom-entry role proof. |
| HOST-08 | Model selection at launch and readback | lanes, roles | SIMPLIFY | Pass the model as a flag and compare OMP's state answer; drop the extension-written proof record. |
| HOST-09 | Fresh start versus exact resume | lanes, lead | KEEP | Resuming the wrong transcript is a correctness failure; exact checks are cheap. |
| HOST-10 | rpc-ui transport (framing, correlation, limits, chunking) | all control | SIMPLIFY | Old tree had two separate clients with different limits; keep one. |
| HOST-11 | Fault and ambiguity handling | all control | KEEP | Never replay an input whose outcome is unknown; this is the core safety rule. |
| HOST-12 | Session status snapshot | lead, messengers, CLI | KEEP | Every caller decides from it; precedence must be fixed. |
| HOST-13 | Recent output ring | lead, CLI | KEEP | Cheap, bounded, enough for "what is it doing". |
| HOST-14 | Pending dialogs and answering them | owner, lead, messengers | KEEP | Approvals and questions must be answerable without a terminal. |
| HOST-15 | Input delivery and wake | messengers, lead, scheduler | KEEP | The main job of the host; must be fast and exact. |
| HOST-16 | Idempotent submissions with receipts | messengers, CLI | SIMPLIFY | Three receipt schemes (host, extension, prototype) become one, and it survives restarts. |
| HOST-17 | Interrupt the running turn | owner, lead | KEEP | Needed and simple. |
| HOST-18 | Change model on a live session | owner | KEEP | Needed and simple with readback. |
| HOST-19 | Goal start, pause and resume | lanes | KEEP | Owner-visible behaviour, but the needed OMP surface is missing; see questions. |
| HOST-20 | Graceful close and stop record | lifecycle, roles | KEEP | Clean exit keeps transcripts resumable. |
| HOST-21 | One change at a time per session; reads never wait | all control | KEEP | Removes races without slowing status reads. |
| HOST-22 | Local control socket | CLI, extension, lead | SIMPLIFY | One owner-only socket with line JSON replaces a per-session HTTP server with bearer keys. |
| HOST-23 | Live event stream and transcript page | CLI, messengers | SIMPLIFY | Served once by the daemon for both runtimes. |
| HOST-24 | Per-session listener inside the extension | CLI | MERGE into HOST-22 | The extension dials the daemon instead of listening. |
| HOST-25 | Open or wake unregistered sessions; separate wake policy file | operator | DROP | Bypasses lane records; already dropped by the lane-lifecycle spec. |
| HOST-26 | Standing role sessions (lead, duty) and resuming the last lead | daemon | SIMPLIFY | Keep the resume rule; the daemon's own record replaces the separate inventory files. |
| HOST-27 | Plain loop roles hosted as their own processes | scheduler | DROP | They become daemon timers (watch-schedule area). |
| HOST-28 | Crash recovery and orphan handling | daemon | SIMPLIFY | Old design left sessions "unknown" forever; use OMP's lease check and reconcile at start. |
| HOST-29 | Capacity cap and memory admission before spawn | daemon | SIMPLIFY | One cap and one memory rule; thresholds live in the lane-lifecycle spec. |
| HOST-30 | Choosing the runtime for a new session | daemon | SIMPLIFY | Config only; drop environment and PATH sniffing. |
| HOST-31 | Tern runtime hosting | owner who wants to watch | SIMPLIFY | Keep visible sessions, but control them through the extension link, not the screen. |
| HOST-32 | Screen reading, paste-and-Enter delivery, process-table identity checks | tern path | DROP | Fragile, slow and replaced by HOST-31. |
| HOST-33 | Parsing arbitrary launch wrappers and verifying a patched OMP source build | every start | DROP | Violates "thin over OMP"; config names one executable. |
| HOST-34 | Stdio controller prototype with a demo host tool | none | DROP | Experimental second host; its useful parts are in HOST-16 and HOST-35. |
| HOST-35 | Health check | owner, installer | KEEP | Quick proof that OMP and rpc-ui work before anything else starts. |
| HOST-36 | Platform differences (Linux, macOS, Windows) | daemon | SIMPLIFY | Fewer branches once the daemon is the parent of every child. |
| HOST-37 | Owner-only state files | daemon | KEEP | Cheap protection for transcript paths and receipts. |
| HOST-38 | Byte-exact compatibility with the old scripting clients | none | DROP | Mirrors another runtime's error text and quirks; no value in a clean product. |
| HOST-39 | Searching for the native binary in several places | CLI | DROP | The product is the one binary. |

## Requirements

### Process ownership (HOST-01, HOST-21)

1. One long-lived daemon process per OS user is the parent of every headless OMP child. No per-session supervisor
   process exists.
2. The daemon talks to each headless child only through the child's stdin (commands) and stdout (responses and
   events). The child's stderr goes to a per-session log file capped at 1 MiB (oldest half dropped when full).
3. Per session, at most one state-changing operation (deliver, answer, goal change, interrupt, model change, close)
   runs at a time. A second one arriving meanwhile is refused at once with "busy: another change is in flight";
   it is not queued.
4. Status reads never wait behind a change, a pending dialog or the start handshake; they return the last
   confirmed state, marked as such.
5. Concurrent callers that need a fresh state share one in-flight get_state.

### Records and start (HOST-02, HOST-09, HOST-37)

6. Before spawning a child the daemon durably records: a new random session handle, role (lead, duty or lane),
   working directory (resolved to a real path), display name, group, requested model, runtime, start time, and
   whether this is a fresh start or a resume (with the expected OMP session id and transcript path).
7. A start that crashes between "recorded" and "identity bound" leaves the record in a starting state. Such a
   record is never started again automatically; recovery (HOST-28) decides.
8. A second start for a handle that is starting, live or stopped is refused.
9. Fresh start: the child gets `--session-dir <per-role or per-lane directory>` so its transcript lands in a known
   place. Resume: the child gets `--resume=<absolute transcript path>`; the path must be an existing regular file.
10. After a resume, the session id OMP reports must equal the expected id and the transcript path must equal the
    requested path after resolving symlinks. Any difference: close the child (HOST-20) and refuse with the two ids.
11. Only lanes and the lead may resume. Duty always starts fresh.
12. State directories are mode 0700 and files 0600, owned by the daemon's user. A record that is a symlink, has
    looser permissions, another owner, or exceeds 4 MiB is refused as unreadable; the daemon does not guess.
13. Records are replaced atomically: write a uniquely named temporary file, flush it, rename over the target, flush
    the directory.

### OMP compatibility and launch (HOST-03 to HOST-06)

14. At daemon start, and whenever the configured OMP executable changes on disk, the daemon runs `omp --version`
    with a 5 s limit and 4 KiB output cap. The output must parse as a version at or above the configured minimum.
    Otherwise no session starts and the reason is reported.
15. The rpc-ui ready frame must state protocol version 1. If it also lists version 2, the daemon negotiates 2
    with negotiate_protocol and accepts rpc_chunk frames; otherwise it stays on 1.
16. OMP features beyond the basic command set are probed when first needed. A refusal is recorded as "feature
    unavailable in this OMP" and the dependent capability reports that reason; it never falls back silently.
17. The OMP command line is built by the daemon from structured config: one executable (absolute path, or a name
    resolved on PATH once at daemon start), plus an owner-approved list of extra flags. No shell, wrapper,
    package runner or environment-prefix command is accepted.
18. Extra flags may not choose mode, session directory, resume, continue, restore, fork, print, goal, plan, approval
    mode, auto-approve or working directory, and may not add initial prompt text. Such a flag fails config
    validation at daemon start, not at session start.
19. The daemon always adds: `--mode rpc-ui`, `--approval-mode always-ask`, `--config <overlay>`, `--model
    <provider/id>`, the optional `--thinking <level>`, the Cataly extension with `-e <path>`, and for role
    sessions `--append-system-prompt <owner-only file>` holding the role instructions. The working directory is
    set as the child's process cwd.
20. The overlay file is written once per daemon run, owner-only, and sets two public OMP settings: questions from
    the ask tool never time out (`ask.timeout` 0), and goals may continue in rpc mode (`goal.continuationModes`
    includes rpc).
21. The child environment starts empty and receives only: PATH, HOME, TMPDIR, LANG, the provider credentials the
    owner listed, the session handle the extension needs to find its record, and the daemon socket path.
    Messenger tokens and Cataly's own secrets are never passed. Secrets never appear in argv.
22. OMP desktop notifications are off for headless children (an OMP environment switch; confirm it is documented).

### Handshake and identity (HOST-07, HOST-08)

23. After spawn the daemon waits at most 15 s for the ready frame. A non-ready first frame, a protocol it does not
    support, or a timeout ends the start: the child is closed (HOST-20) and the record marked failed with reason.
    After a ready timeout on an automatic lead resume, a reaped child is eligible for the lead's existing
    restart backoff. Each retry checks the lease before resuming the same transcript. An unbound failed
    resume attempt does not supersede the session it tried to resume.
24. Then it sends get_state. The answer must carry a non-empty session id and an absolute transcript path. A
    second get_state must report the same pair. The pair is then bound to the session handle.
25. The transcript file may be absent until OMP writes the first entry. If it is still absent at the first idle
    readback after a completed turn, the session is faulted.
26. Every later get_state answer is compared with the bound pair. A different session id or path means OMP switched
    transcripts: the session becomes faulted (HOST-11) and receives no further input.
27. The model OMP reports in get_state must equal the requested provider/id (provider exact, id case-insensitive),
    and the reported thinking level must equal the requested one when one was requested. If not, the session
    stays open but held with reason "model mismatch"; no input is delivered and no goal is set until the owner
    closes it or explicitly accepts the observed model.
28. Start returns only after steps 23-27 complete or fail. Total start budget: 30 s. Callers are notified by event,
    not by polling.

### Transport (HOST-10, HOST-11)

29. Frames are one JSON object per line. A line over 1 MiB (or the smaller limit the ready frame announces), a
    non-object line, invalid JSON, or a broken chunk sequence ends trust in the stream.
30. Each command carries a unique id. A response is accepted only if its id is pending and its echoed command name
    matches; anything else is ignored. A response not received within 15 s is "unconfirmed".
31. A write to the child's stdin that does not finish within 15 s ends trust in the stream (a half-written line
    would corrupt every later one).
32. An rpc_frame_error frame from OMP ends trust in the stream (a lost UI frame could hide a pending approval).
33. When trust ends: all pending commands fail as unconfirmed, the session is faulted, its status becomes error,
    and only close and status remain available.
34. If a state-changing command was written and its outcome is unconfirmed, the result is "ambiguous: do not resend".
    The daemon never retries such a command itself.

### Status (HOST-12, HOST-13)

35. A snapshot contains: handle, role, runtime, cwd, start time, update time, status, an input-blocked flag, OMP
    child pid, bound session id and transcript path, goal state, model, current turn start, last fault or hold
    reason, pending dialogs, and the output ring.
36. Status precedence: error (faulted) beats blocked (pending dialog, or goal blocked by budget) beats unknown (not
    yet bound, stopping, or no fresh readback) beats idle beats working.
37. Idle means OMP reports no streaming, no compaction and no queued messages. Working means any of those.
38. Input is blocked on: pending dialog, fault, stopping, unbound, model mismatch, or no fresh readback.
39. Identity fields (session id, path, goal, model) appear only for a bound, non-faulted session.
40. The output ring keeps the last 40 of: final assistant text of a message, command output, OMP notify texts.
    Each is cut to its last 2000 characters, control characters other than newline and tab removed, blanks skipped.
41. The current-turn start is set on the first agent_start after idle and cleared when a readback shows idle.

### Dialogs (HOST-14)

42. Every extension_ui_request frame is classified: select (an approval when its options include Deny and an
    approve option), confirm, input (secret when OMP marks it so), editor, a multi-question form if OMP sends one,
    or unsupported (any other method, URL-open included). Cancel frames remove the named dialog. Notify, status,
    widget, title and editor-text frames are not dialogs.
43. A dialog id seen again replaces the earlier one in place.
44. An answer must name the bound session id and the dialog id exactly; otherwise it is refused.
45. Allowed answers: cancel (any supported kind); confirm yes or no; a select value that is one of the offered
    options; for approvals only Deny or a single-call approval (never a session-wide or permanent grant); input
    or editor text; for ask, one answer per question in order, each choosing offered options without repeats,
    at most one choice unless the question allows several, optional free text, at least one choice or text.
46. Secret inputs and unsupported dialogs cannot be answered remotely; they wait for the owner at a terminal or
    the session is closed.
47. Fields OMP might treat specially (for example a timed-out flag that accepts recommended answers) are never
    forwarded. Exactly one answer field is sent.
48. The dialog is removed from the pending list before the answer is written. If the write is not confirmed the
    session is faulted and the caller is told "ambiguous; do not resend".

### Input delivery and wake (HOST-15)

49. A delivery names the session handle, the expected OMP session id, the text, and a timing rule: only when idle
    (default), only while working, or either.
50. Checks, in this order, against a fresh readback: bound identity matches the expected id; not faulted, stopping
    or held; no pending dialog; goal not paused or blocked; text not blank; text at most 256 KiB; the timing rule
    matches the current status. A failed check returns "not ready" with the current status and changes nothing.
51. Wake: if the session is idle the daemon sends prompt (with follow-up as the streaming behaviour, so a turn that
    starts in between queues the text instead of failing). This starts a turn. If working, it sends follow_up,
    which OMP runs after the current turn.
52. A steer delivery (explicitly requested) sends steer and requires a running turn; with none it is refused.
53. Text whose first non-space character is "/" is sent with a short fixed first line marking it as a message, so
    OMP does not run it as a command. Cataly never passes OMP commands through delivery.
54. Results: delivered (and whether a new turn was started), not ready (status), refused (reason), or ambiguous.
55. Latency target: one readback plus one command round trip; no sleep or poll in the path. The daemon records the
    time from accepting a delivery to the next agent_start event and exposes it for measurement.

### Receipts (HOST-16)

56. A caller may attach a request id: 1-128 characters, starting with a letter or digit, then letters, digits, dot,
    underscore, colon or hyphen.
57. The daemon keys receipts by session and request id, with a digest of the delivery kind and text. The same id
    with the same digest returns the stored receipt and sends nothing. The same id with a different digest is
    refused.
58. A receipt is pending until OMP confirms admission, then admitted, or refused with reason. An unconfirmed
    admission stays pending with the reason "unconfirmed; do not submit a new id".
59. At most 512 receipts per session are kept. When full, new ids are refused; old receipts are never evicted while
    the session lives.
60. Receipts are stored durably. After a daemon restart a receipt that was pending becomes "unconfirmed" and is
    never resubmitted. Receipts are deleted with the session record.
61. A receipt states that admission happened; it never claims which reply belongs to it.

### Interrupt, model, goal (HOST-17 to HOST-19)

62. Interrupt requires a running turn (streaming or compacting); otherwise it is refused as "nothing to interrupt".
    It sends abort, then a readback must show the turn stopped; else the result is "abort sent, stop unconfirmed".
    Background jobs are not targeted and the result says so.
63. Model change requires an idle, unheld session and a selector of the form provider/id (no spaces, at most 256
    characters). If already on that model it succeeds without a command. Otherwise it sends set_model, then the
    readback must show the requested model. OMP refusal is reported as refused; no confirmation is ambiguous.
64. Goal start requires an idle session with no goal, a non-blank objective, and a matched launch model (step 27).
    OMP must answer with an active goal. Pause on a working session also interrupts the turn and must read back
    idle. Resume must read back active. A goal blocked by budget needs the owner and is refused here.
65. The installed OMP (19.1.1) has no rpc-ui command for goals. Until a public surface exists, step 64 reports
    "feature unavailable in this OMP" (see questions).

### Close (HOST-20)

66. Close requires the bound session id, an idle session and no pending dialog. A forced close first interrupts and
    waits up to 15 s for idle; if a dialog appears meanwhile it refuses.
67. Close ends the child's stdin. OMP's rpc mode answers end of input by finishing owed responses, cancelling
    dialogs, shutting the session down and exiting.
68. The daemon waits up to 15 s for exit. If the child is still running it sends SIGTERM, waits 5 s more, then
    SIGKILL. The stop record says which step ended it.
69. The stop record holds: how it ended (clean close, forced, child exited by itself with code or signal, failed
    start with reason), the closed session id, and the time. A caller that asked to close gets its answer only
    after this record is written.
70. A child that exits without being asked marks the session stopped with its exit code, raises an event, and
    leaves the transcript for a later resume.

### Control socket, events (HOST-22 to HOST-24)

71. The daemon listens on one Unix socket in a directory it created with mode 0700; the socket is 0600. The path
    must fit the platform limit (103 bytes on macOS, 107 on Linux); if the preferred runtime directory is too long
    the daemon uses a short directory under /tmp.
72. Every connection's peer user id must equal the daemon's; others are closed without a reply.
73. Requests and replies are single JSON lines; a request over 1 MiB closes the connection with an error line. An
    idle connection is closed after 60 s; a request runs to completion even if its caller disconnects.
74. At most 64 concurrent connections; further ones are refused.
75. Before binding, a stale socket file is removed only if connecting to it is refused; a live one means another
    daemon runs, and this one exits with that message.
76. Event stream: per session the daemon keeps the last 256 events, numbered by a sequence that only increases. A
    subscriber passes the last number it saw and gets later events (pages of at most 40 when reading on demand).
    If that number is older than the oldest kept event, the reply says events were missed.
77. Events: turn started, turn ended (with whether OMP will continue), message (role and visible text only, cut to
    1536 characters with a truncated flag), dialog opened or closed, submission settled, model changed, interrupt
    unconfirmed, session stopped.
78. Each child start, and each session switch OMP reports, gets a new view token. Callers acting on a view pass the
    token; a mismatch is refused so nobody acts on a stale view.
79. Transcript page: the last N (at most 40) visible messages of the current branch, same text rules as events.
    Reasoning, tool inputs and tool results are never included.

### Standing role sessions (HOST-26)

80. On daemon start the lead is started, then duty. Each must bind a session id distinct from every other live
    session; otherwise start stops and nothing already running is replaced.
81. The lead resumes its last session only if that session's stop record is a clean close of exactly that id and
    the transcript still exists; otherwise it starts fresh and says why. The owner may force fresh.
82. A deployment may require a specific lead session id. Then the lead must resume exactly that one or the daemon
    refuses to start the lead; a fresh lead is never substituted.
83. Stopping the roles refuses if a role is working or has a pending dialog. The lead is stopped before duty.

### Recovery (HOST-28)

84. If the daemon dies, each headless child sees end of input and shuts down on its own (OMP behaviour); transcripts
    remain on disk.
85. On start the daemon reads every record. For records that were starting or live, it runs `omp session lease
    --json -- <ids>` (10 s limit) for their session ids.
86. Held by a running OMP: the record is marked orphan-held; the daemon does not resume it and reports it. The lease
    is checked again when someone tries to start it.
87. Not held: the record is marked lost (host exited) and becomes eligible for resume. Lanes are not resumed
    automatically (the lane-lifecycle spec decides); role sessions follow step 81.
88. Lease check failed or answered unknown: the record is held for the owner. The daemon never starts a second
    process on a transcript it cannot prove free.

### Capacity (HOST-29)

89. A configured cap (default 8, range 1-32) limits live OMP children, roles included. A start beyond it is refused.
90. Before every spawn the daemon applies the memory admission rule of the lane-lifecycle spec. An unreadable
    memory reading refuses the start.

### Runtimes (HOST-30, HOST-31)

91. The runtime for new sessions comes from config: headless (default) or tern. A stored session always stays on
    the runtime it was started with.
92. If tern is configured but its executable is missing, starts are refused with an install hint; there is no
    automatic fallback.
93. Tern start: the daemon asks Tern to open a new block (new session, tab or split as requested) running the OMP
    executable directly with the same flags as headless except mode and approval mode (interactive TUI, the
    owner's approval mode).
94. The Cataly extension in that OMP connects to the daemon socket at session start and registers session id,
    transcript path, OMP pid and cwd, presenting a random per-start token the daemon placed in that block's
    environment. A missing or unknown token, or one already bound to another pid, is refused.
95. Through that link the daemon delivers input (idle: start a message; working: follow-up or steer), interrupts,
    changes model and requests shutdown using the public extension API. Status comes from the extension's idle,
    pending-messages and job-snapshot answers plus approval and turn events.
96. Dialogs in tern belong to the human at the terminal. The daemon sees approval requested and resolved events,
    reports the session as blocked, and never answers.
97. If the link drops, status becomes unknown and input is refused until the same OMP process (same pid and token)
    reconnects with the same session id. A different session id means a new identity: the old one is ended.

### Health check (HOST-35)

98. The check runs `omp --version` (5 s), then starts an ephemeral rpc-ui child with no session, no extensions,
    skills, rules, context files or tools, offline. It reads the ready frame (protocol, frame limits), checks that
    get_state reports no transcript, probes host tool support by registering an empty tool list, then closes by
    end of input. Whole check: 20 s. Output: version, protocol, limits, host tool support, pass or fail with reason.

### Platforms (HOST-36)

99. Releases: linux-x64 and darwin-arm64, same behaviour on both.
100. Signals go only to the daemon's own unreaped children, so a reused process id can never be hit; no process
     table reading is needed.
101. Memory reading: Linux uses available versus total memory and the tightest visible cgroup limit; macOS uses the
     kernel's memory level (`kern.memorystatus_level`), falling back to free versus total pages.
102. Windows: no daemon release. Windows keeps working as it does today on its own path; nothing in this spec adds
     Windows behaviour (see questions).

## Third-party interfaces used

OMP public surfaces (installed 19.1.1 checked):
- CLI: `--mode rpc-ui`, `--approval-mode always-ask`, `--config`, `--model`, `--thinking`, `--session-dir`,
  `--resume=<path>`, `--no-session`, `--append-system-prompt`, `-e`, `--no-extensions`, `--no-skills`,
  `--no-rules`, `--no-context-files`, `--no-tools`, `--offline`, `--version`, `omp session lease --json`.
- Settings in an overlay: `ask.timeout`, `goal.continuationModes`.
- rpc-ui commands: negotiate_protocol, get_state, prompt (with streamingBehavior), steer, follow_up, abort,
  set_model, set_host_tools (health check only). Frames: ready, response, rpc_chunk, rpc_frame_error, events
  agent_start, agent_end, message_end, extension_ui_request, extension_ui_response. End of stdin as shutdown.
- Extension API: flags, appendEntry, session_start, session_switch, session_shutdown, agent_start, agent_end,
  message_end, tool_approval_requested and resolved, goal_updated, sendUserMessage with deliverAs steer or
  followUp, setModel, abort, shutdown, isIdle, hasPendingMessages, getAsyncJobSnapshot, session id, transcript path
  and branch reading.
- Tern CLI: create session, tab or split with a program; close a block.

Not present in the installed OMP's public surfaces (the old design relied on them; the new product must not):
an rpc command listing custom transcript entries; an rpc toggle for ask dialogs; rpc goal create, pause and
resume; an rpc fork command; a settled flag in get_state and a settled event; prompt results carrying status and
settled fields; an ask dialog method; and, in the extension API, a pending plan review query, plan review events, a
reset-boundary entry type, an "aside" delivery kind, a guaranteed write-to-disk call, and a waiting-for-background
flag on turn end. The old tree also pinned OMP 19.0.1 built from source with a local patch.

## State that must survive a restart

- Per session: handle, role, runtime, cwd, name, group, requested model and thinking, start kind, bound session id
  and transcript path, lifecycle state (starting, live, stopped, lost, orphan-held, failed) and stop details.
- Per session: the submission receipts and their states.
- The last cleanly stopped lead session (id and transcript path) and any required lead session id.
- Config: OMP executable, minimum version, allowed extra flags, capacity cap, default runtime.
- Not kept: output ring, events, dialogs, view tokens (rebuilt from the live child).

## Problems in the old approach, and better ideas

1. Process count: one detached supervisor per session (each with a small HTTP server), the two plain loops as
   separate processes, and a separate prototype controller. Better: one daemon, children over pipes.
2. Duplication: the host existed in the scripting runtime and again in Rust, kept byte-identical down to the other
   runtime's error texts and string quirks, beside a second independent rpc client. Better: one implementation.
3. Reliance on OMP internals: a patched OMP 19.0.1 built from source with an exact pin, rpc commands and fields the
   installed OMP lacks, and extension members outside the public API. Better: minimum version plus probing.
4. Circular identity proof: the extension wrote role and launch-model records that the host read back to prove what
   it had launched itself. Better: the daemon knows what it spawned; OMP's state answer gives model and identity.
5. Polling: start re-read status every 50 ms for up to 65 s, stop every 20 ms, remote open every 200 ms, remote close
   20 times at 100 ms. Better: child exit and frame events wake waiters.
6. A hand-written HTTP/1.1 parser with key files and random socket directories per session, plus a second listener
   inside the extension for the same session. Better: one line-JSON socket with a peer-uid check.
7. The tern path read the screen to find OMP's input box, pasted text and pressed Enter up to three times, forced
   the classic TUI, and proved identity from the process table. Better: the extension links to the daemon.
8. A signal-killed host left sessions "unknown" forever, and a stalled exit was never forced. Better: lease check at
   start and bounded stop escalation.
9. Hundreds of lines parsed wrappers, env options and launchers to recognise OMP in a command string; receipts died
   with their process; three memory thresholds in three places. Better: one executable in config, durable receipts,
   one memory rule.

## Questions for the owner

1. Goals: OMP 19.1.1 has no rpc-ui command to start, pause or resume a goal (only the terminal /goal). Run lanes
   without OMP goal mode (the daemon delivers the next instruction), or wait for a public upstream surface?
2. Stuck exits: may the daemon SIGTERM, then SIGKILL, an OMP child still running 15 s after end of input? The old
   product never did and left such sessions stuck.
3. Windows: should the new Cataly build for Windows at all (tern only, named pipe instead of a Unix socket), or does
   the existing product keep serving Windows for now?
4. After a daemon crash, may the lead resume its last session when the lease shows it free though its stop was not
   clean? And is a default cap of 8 live OMP children right?
