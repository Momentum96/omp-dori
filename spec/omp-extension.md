# OMP extension: behaviour spec

Behaviour spec used to write Cataly 0.2; see docs/provenance.md.

## Purpose

Cataly needs a small piece of TypeScript inside every OMP process it cares about, because some things can only be
done from inside OMP: offering tools to the agent, vetoing a tool call before it runs, adding text to the system
prompt, refusing a provider request, holding a session open when it tries to stop, and seeing tool results and
approval prompts as they happen. Everything else belongs in the Rust daemon.

Users: the lead session and lane sessions (they get tools and guidance), the daemon (receives facts, sends
commands), the owner (indirectly: approvals, safety holds), and ordinary OMP sessions (must be unaffected).

Today the extension is about 6,000 lines that hold a great deal of the product: it runs background commands,
polls a messenger inbox every 3 seconds, keeps its own Discord connection for some lanes, writes per-session state
files and a health file every second, runs a local control socket server, re-implements report acceptance, and
checks transcripts on disk to prove message delivery. This spec keeps the observable guarantees and moves the
logic to the daemon, leaving a thin forwarder. The last part of Requirements lists what must remain.

Related areas: the background-job tool is specified in watch-and-schedule (WS-02), report and evidence rules in
evidence-reporting (EVR-01 to EVR-14), message hand-off to the lead in discord-inbound (DISC-IN-15), lane identity
and shell rules in lane-lifecycle (LANE-05, LANE-14, LANE-30), owner questions and the stop reminder in
discord-outbound (DISC-OUT-09, DISC-OUT-15 to DISC-OUT-19). This spec states only the extension's share.

## Capabilities

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| EXT-01 | Loaded by every OMP process, inert unless the session belongs to Cataly | all sessions | KEEP | Global install is how OMP finds it; ordinary sessions must pay nothing. |
| EXT-02 | Session role: set at launch, remembered in the session, restored on resume and branch moves | lead, lanes | SIMPLIFY | Session marker is only a hint; the daemon decides the role by session identity. |
| EXT-03 | Role carried into a fresh session in the same process (new session command) | lead, lanes | KEEP | Otherwise one command silently drops a session out of Cataly. |
| EXT-04 | Subagents inherit the parent's role, including workers in a separate realm that read the parent transcript | lanes | SIMPLIFY | Needed only for shell rules; the daemon resolves lineage from the parent session ID. |
| EXT-05 | A restricted second lead-type role with its own guidance and a role receipt in the transcript | lead helper | DROP | The daemon knows which session is the lead; every other session is not. |
| EXT-06 | Per-terminal-block state file (session, goal, approvals, working/idle, jobs, model readback), left as an exit marker when the process ends | CLI, watchdog | SIMPLIFY | Facts go over the daemon connection as events; no file at all. |
| EXT-07 | Guarding the state file against nested OMP runs that inherit the block variable (pid probe, ps start time) | state file | DROP | A single-use launch token and the daemon's own process table remove the problem. |
| EXT-08 | Goal status mapping (budget-limited counts as blocked; unknown never reads as "no goal") | daemon | MERGE into EXT-10 | The daemon maps raw OMP goal states; the extension forwards them unchanged. |
| EXT-09 | Open tool approvals tracked per session and tool call | lead, lanes, daemon | MERGE into EXT-10 | Forwarded as events; the daemon keeps the set. |
| EXT-10 | Event forwarding to the daemon (session, goal, agent, turn, message, tool, approval, plan review, jobs) | daemon | SIMPLIFY | One typed stream on one connection replaces files, a socket server and polling. |
| EXT-11 | Tool visibility per role and agent kind (lead tool, lane tool, goal tool, nothing for subagents) | lead, lanes | SIMPLIFY | Keep the visibility rule; drop the goal-tool dance once the daemon sets goals. |
| EXT-12 | Role guidance appended to the system prompt of root sessions every turn | lead, lanes | SIMPLIFY | Keep a short block; its text comes from the daemon, not from extension source. |
| EXT-13 | Extra prompt block for one model family about batching bookkeeping | lanes | DROP | Model-specific patch; same as EVR-37. |
| EXT-14 | Exact launch model guard with readback (requested, observed, billing, thinking level) | lanes | SIMPLIFY | Keep a local, fail-closed barrier on provider requests; the comparison rules live in the daemon. |
| EXT-15 | Lane report tool, with registration, ownership and run checks inside the extension | lanes | SIMPLIFY | A forwarding tool; acceptance belongs to the daemon (EVR-01, EVR-11). |
| EXT-16 | Tool-call observation for evidence (start directory, duplicate IDs, background job completion binding) | report tool | SIMPLIFY | Keep only what needs the live branch: fingerprint and completion facts. |
| EXT-17 | Background command tool with batching, filters, restarts, silence alerts, oversize spill and resume restore | lead | MERGE into WS-02 | Jobs run in the daemon; the extension tool forwards. |
| EXT-18 | Private start-up file that installs a set of background commands into a session | managed start | DROP | The daemon owns its jobs; no session replay (WS-04). |
| EXT-19 | Mirror file of the session's background commands for the next session | managed start | DROP | Same reason. |
| EXT-20 | Message injection into the session (wake now, next turn, aside, steer) | daemon | KEEP | Only the extension (or rpc) can put text in front of the agent. |
| EXT-21 | Exactly-once admission of injected messages with identity, fingerprint and on-disk receipt | lead | SIMPLIFY | Keep the tag and the entry ID answer; the daemon does the ledger and the disk search (DISC-IN-15). |
| EXT-22 | Content filter re-check at hand-off and a three-step ready/send/check exchange with child processes | lead | DROP | Existed because a child process spoke through the extension. |
| EXT-23 | Recording an owed reply before admitting a line | lead | MERGE into DISC-IN-21 | The daemon records obligations when it routes a message. |
| EXT-24 | Stop gate: keep the lead running while owner replies are open, bounded retries | lead | MERGE into DISC-OUT-09 | The extension forwards the stop and returns the daemon's verdict. |
| EXT-25 | Local control socket per session (info, transcript page, event page, send, steer, interrupt, model, close) | CLI, daemon | SIMPLIFY | Reverse it: the extension is a client of the daemon; commands arrive on that connection. |
| EXT-26 | Cancel a session switch while a remote action is in flight; detect a transcript clear | daemon | KEEP | Prevents a command landing in the wrong session; small. |
| EXT-27 | Messenger lane intake inside the extension (3 s inbox poll, delivery, final-answer capture, reply queueing) | lanes | MERGE into DISC-IN-14 | The daemon delivers to thread sessions through EXT-20; no polling. |
| EXT-28 | A lane taking over a separate messenger connection of its own | lanes | DROP | A second Discord client inside OMP; one daemon connection serves all. |
| EXT-29 | Special case for a goal that a cold terminal start pauses before a messenger wake | lanes | DROP | Old wake path through a terminal resume; the daemon resumes sessions itself. |
| EXT-30 | Mirroring OMP's interactive form to Discord and returning the answer (ASK) | lead, lanes | MERGE into DISC-OUT-15 | Extension share: forward the form, apply the answer. |
| EXT-31 | Per-lane shell command rules checked on every shell call, fail closed, inherited by subagents | lanes | SIMPLIFY | Rule evaluation moves to the daemon (LANE-30); the hook stays. |
| EXT-32 | Refuse spawning a subagent when the parent is not on disk and a rule set applies | lanes | KEEP | Without a parent file the child could escape the rules. |
| EXT-33 | Installation discovery: home, configuration file and state directory found from the extension's real path | all | SIMPLIFY | The extension needs only the daemon's socket address. |
| EXT-34 | Health file written every second with process identity, source hash and child jobs | health check | DROP | Connection liveness and a version handshake replace it (WS-05). |
| EXT-35 | Taking ownership of a resumed transcript before announcing the session (append, then flush to disk) | daemon | KEEP | Ensures the announced session ID is the one this process writes. |
| EXT-36 | Status bar text and one-time warnings | lead, lanes | SIMPLIFY | One status slot whose text the daemon supplies; warnings on connection loss only. |
| EXT-37 | Recovery summary pushed at session start and after compaction | lead | MERGE into EVR-28 | Delivered through EXT-20. |

## Requirements

### Where each capability lives

"Inside OMP" names the extension API surface the capability depends on. "Daemon" says whether the decision
logic can move to Rust, leaving the extension as a forwarder.

| ID | Inside OMP | Daemon |
| --- | --- | --- |
| EXT-01 | extension loading; session_start, session_switch | Extension only decides "candidate or not"; acceptance in daemon. |
| EXT-02 | registerFlag, getFlag; appendEntry; session branch read | Yes. Role table keyed by session ID; extension writes the marker. |
| EXT-03 | session_switch with reason "new" | Yes. Daemon links the new session ID; extension re-appends marker. |
| EXT-04 | agent kind and parent ID; session header parent | Yes. Daemon resolves lineage; extension sends parent ID. |
| EXT-05 | before_agent_start | Dropped. |
| EXT-06 | goal_updated, agent_start/end, approval events, job snapshot | Yes. Events only. |
| EXT-07 | none (process table, environment) | Dropped. |
| EXT-08 | goal_updated | Yes. Mapping in daemon. |
| EXT-09 | tool_approval_requested, tool_approval_resolved | Yes. Set kept in daemon. |
| EXT-10 | the events listed in requirement 11 | Yes. Extension serialises and sends. |
| EXT-11 | registerTool (inactive default), getActiveTools, setActiveTools | Rule in daemon; extension applies the list. |
| EXT-12 | before_agent_start system prompt replacement | Text in daemon; append must stay in the extension. |
| EXT-13 | before_agent_start | Dropped. |
| EXT-14 | before_provider_request, before_agent_start, abort, model registry, thinking level | Pin and comparison rules in daemon; the barrier must stay local (no round trip per request). |
| EXT-15 | registerTool | Yes, entirely. |
| EXT-16 | tool_execution_start/update/end; session branch read | Refusal rules in daemon; fingerprinting needs the live branch, so it stays. |
| EXT-17 | registerTool; sendMessage | Yes, entirely (WS-02). |
| EXT-18 | registerFlag; appendEntry | Dropped. |
| EXT-19 | none (file) | Dropped. |
| EXT-20 | sendMessage, sendUserMessage | Decision in daemon; the call must be made inside OMP (or over rpc). |
| EXT-21 | message_end; session flush to disk | Ledger and file search in daemon; extension returns entry ID. |
| EXT-22 | message_end | Dropped. |
| EXT-23 | none | Yes (DISC-IN-21). |
| EXT-24 | session_stop continue result | Decision in daemon; hook stays. |
| EXT-25 | sendUserMessage, abort, setModel, shutdown, idle checks | Yes; commands arrive on the client connection, no server in OMP. |
| EXT-26 | session_before_switch/branch/tree cancel; reset boundary in branch | Small local guard; must stay. |
| EXT-27 | sendUserMessage, agent_end, before_provider_request | Yes, entirely (DISC-IN-14, DISC-OUT-07). |
| EXT-28 | none | Dropped. |
| EXT-29 | goal_updated | Dropped. |
| EXT-30 | tool_call before the form tool runs; tool_execution_end | Routing and exchange in daemon; interception stays for tern. |
| EXT-31 | tool_call block result | Rule evaluation in daemon; hook stays. |
| EXT-32 | before_subagent_spawn block result | Local check; stays. |
| EXT-33 | none | Yes; only the socket address remains. |
| EXT-34 | none (timer, file) | Dropped. |
| EXT-35 | appendEntry; session flush to disk | Must stay in the extension. |
| EXT-36 | ui setStatus, ui notify | Text from daemon. |
| EXT-37 | sendMessage | Yes (EVR-28). |

### Loading and identity (EXT-01, EXT-02, EXT-03, EXT-04, EXT-35)

1. The extension is installed once per user in OMP's global extension location and is loaded by every OMP
   process. In a process that is not a Cataly session it registers no event handlers beyond the ones needed to
   decide that (session start, session switch), starts no timers, opens no connection and adds no prompt text.
   Some subscriptions alter OMP for the whole process just by existing; subscribing to approval events, for
   example, switches off OMP's read-ahead of file reads. Those are added only once the daemon has accepted the
   session (requirement 4).
2. A process is a Cataly candidate when either (a) it was started with the Cataly launch flag carrying a single-use
   launch token, or (b) the current session branch contains the Cataly session marker entry. Neither is proof: the
   daemon decides.
3. On a candidate's first session event in a main agent, the extension connects to the daemon (local socket,
   connect timeout 1 s) and sends a hello: protocol version, OMP version, launch token if any, session ID, session
   file path, process ID, working directory, agent kind, and run mode (interactive or rpc).
4. The daemon answers with a role (lead, lane, or none), the lane key if any, the guidance text (EXT-12), the
   pinned model if any (EXT-14), whether shell rules apply (EXT-31), and the tools to show. "None" makes the
   extension inert for that session: it closes the connection and behaves as in requirement 1.
5. A launch token is valid for one hello only. A nested OMP run that inherits the launch flag or environment
   presents a used token and is told "none". This replaces every process-table check.
6. After the daemon accepts a session with role lead or lane, the extension appends the Cataly session marker to
   the session (once per branch) so a later resume by hand is recognised by rule 2(b).
7. Before sending the hello for a resumed session, the extension appends one small entry and asks OMP to write the
   session to disk. OMP does not claim a resumed transcript until something is appended to it, and if a different
   live process already writes that transcript, OMP gives this process a new session ID. The hello therefore
   carries the ID observed after the flush. If the flush fails, the extension does not connect and shows one
   warning.
8. On session switch, branch move and tree move, the extension sends a transition event with the new session ID
   and file. The daemon answers with the role for the new session (rule 4). A switch caused by the new-session
   command keeps the previous role: the extension re-appends the marker to the fresh session and the daemon links
   the new session ID to the same lane or lead record (lane-lifecycle requirement 38).
9. Subagents: in the same process, a subagent's events carry agent kind "sub"; the extension sends them on the
   parent's connection, tagged with the subagent ID. A subagent running as a separate worker opens its own
   connection and its hello names the parent session ID from the session header; the daemon applies the parent's
   role for shell rules only. If the parent cannot be determined, the daemon answers "unknown parent" and, when the
   parent lineage has shell rules, every shell call of the worker is blocked (rule 33).
10. If the daemon is unreachable at hello time, a candidate retries with backoff 1, 2, 4, 8, 16, 30, 30 ... seconds
    and shows one warning. Until accepted it behaves as a role session with no tools and with shell calls blocked
    when its marker says lane (fail closed), and as an ordinary session otherwise.

### Event forwarding (EXT-06, EXT-08, EXT-09, EXT-10)

11. For an accepted session, the extension forwards these OMP events, each with a per-connection sequence number
    and the session ID: session start, switch, branch, tree, shutdown; goal updated (raw status and objective);
    agent start and end (with OMP's "will continue" and "awaiting background work" flags); turn start and end;
    message end (role, visible text only, stop reason, entry ID, timestamp); tool execution start and end (tool
    name, call ID, working directory, error flag, background job ID and state); tool approval requested and
    resolved; plan review requested and resolved; and the background job snapshot when it differs from the last
    one sent (job IDs, start and end times, states only).
12. Visible text forwarded with message end is limited to user and assistant text and displayed custom messages,
    truncated at 2 KiB with a truncation flag. Reasoning, tool inputs, tool results and hidden custom data are never
    forwarded, except the evidence fingerprints of EXT-16.
13. Events of one connection are sent in OMP's order. While disconnected, the extension buffers at most 1,000
    events or 1 MiB, whichever comes first; on overflow it drops the oldest and sends one "dropped N" event after
    reconnecting. The daemon then treats derived state (busy, approvals, goal) as unknown until the next snapshot.
14. On reconnect the extension first sends a snapshot: role, session, goal, working or idle, open approvals,
    pending plan reviews, job snapshot, current model, and the sequence number of the last event it sent.
15. Heartbeat every 15 s in each direction. The daemon treats 45 s without traffic as a closed connection
    (lane-lifecycle requirement 68). The extension treats it the same way and starts reconnecting.
16. On session shutdown of the main agent the extension sends the shutdown event, waits up to 500 ms for it to
    be written to the socket, and closes. It never deletes or rewrites any file.

### Tools and visibility (EXT-11, EXT-15, EXT-17)

17. The extension registers its tools once at load, inactive by default. Which ones are active follows the
    daemon's answer for the current session and agent: the lead's main agent gets the background-job tool; a lane's
    main agent gets the report tool; subagents and role-less sessions get neither. Visibility is re-applied on every
    session transition, and any of the extension's tools found active in a subagent is removed.
18. Each tool call is forwarded to the daemon with the session ID, agent ID and tool call ID, and the daemon's
    answer text is returned unchanged. Timeout: 10 s for the report tool, 5 s for the background-job tool. On
    timeout or disconnect the tool returns an error stating that nothing was recorded or started, unless the
    daemon later reports otherwise; a retry is safe because the daemon deduplicates by tool call ID.
19. The extension never checks lane registration, run identity or ownership for a report. The daemon does, based
    on the connection's accepted identity (evidence-reporting requirement 3).
20. The goal tool is not managed by the extension. The daemon sets the lane's goal through OMP's goal feature at
    launch and reopen (LANE-01); see question 3.

### Tool evidence (EXT-16)

21. For a lane's main agent, the extension remembers, per tool call ID seen starting in this connection, the tool
    name and working directory at start. A call ID seen starting twice in the same session is marked ambiguous and
    is never evidence.
22. When the report tool names tool call IDs, the extension reads the current branch, finds each call's result,
    and computes its fingerprint (evidence-reporting requirement 13), then sends the IDs with fingerprint, tool
    name, start directory, error flag, and whether it saw the start in this connection. Refusal rules are the
    daemon's (evidence-reporting requirement 12).
23. Background jobs: when a tool call returns a running background job, the extension binds the call to that job
    ID. A completion update for the job (completed, failed, cancelled) yields a completion fact for the call, with
    the fingerprint of the completed result. An update that arrives before the call returns its job ID is held
    until the ID is known and is dropped if the branch has moved past the point where it was seen. Completion facts
    are forwarded as events; they are not written into the session.
24. All per-call memory is cleared on session transitions and is bounded to the newest 4,096 calls per session.

### Messages into the session (EXT-20, EXT-21, EXT-26, EXT-37)

25. The daemon may send a deliver command: text (at most 64 KiB; the daemon spills larger bodies to a file
    first), mode (wake now, next turn, steer the running turn), and a delivery ID. The extension uses OMP's
    send-message call with a custom message type that displays the text and carries the delivery ID in hidden
    details; "wake now" triggers a turn as an aside; "next turn" does not trigger; "steer" uses the user-message
    call with steer delivery and is refused unless a turn is running.
26. The extension answers "refused, not current" without calling OMP when the command's session ID differs from
    the current session, a transition is in progress, or the command arrived before the hello was accepted.
27. When OMP reports message end for a custom message carrying a delivery ID, the extension asks OMP to flush the
    session to disk and then sends "delivered" with the entry ID and session file. The daemon confirms by finding
    that entry in the file (DISC-IN-15). The extension never reads the session file itself.
28. If OMP's send call throws, the extension answers "refused" with the error text. If no message end arrives
    within 30 s, it sends nothing more; the daemon treats the delivery as in doubt (discord-inbound requirement 50).
29. Delivery commands are executed one at a time per session, in arrival order. A repeated delivery ID with the
    same text and mode returns the earlier outcome without calling OMP again; with different text it is refused.
    The extension remembers the newest 1,024 delivery IDs per session. Whether a delivery should wait for an open
    approval, form or plan review, or for a paused goal, is the daemon's decision; it knows them from the events.
30. Control commands from the daemon: interrupt (abort the running turn; answered "idle" if none), set model
    (resolve through OMP's model registry, set it, read it back; refused while a turn is running), and shut down
    (graceful OMP shutdown; refused while busy unless forced). Each answers with what OMP reported.
31. While a control or delivery command is being applied, a user-initiated session switch, branch or tree move
    is cancelled with a one-line notice. When OMP clears the transcript without a session event (a new reset
    boundary appears at agent start), the extension sends a transition event so the daemon drops stale state.

### Shell rules (EXT-31, EXT-32)

32. When the daemon says shell rules apply, every shell tool call of the session's main agent and subagents is
    sent to the daemon before it runs (command text, session, agent, call ID). The daemon answers allow or block
    with a reason (LANE-30). Timeout 2 s. The extension's hook runs before OMP's own approval prompt.
33. Any failure blocks the call: timeout, disconnect, malformed input (no command string), unknown parent
    lineage, or a daemon error. The block reason says no command ran and asks the agent to report a blocker.
    The veto is returned as OMP's block result from the hook; a thrown error is never relied on to stop a call.
34. When shell rules apply and the session is not yet on disk, spawning a subagent is refused with a reason, so
    the child cannot start without a parent file to inherit from.
35. Sessions without shell rules pay no round trip: the hook returns immediately.

### Prompt guidance and model barrier (EXT-12, EXT-14)

36. Before each agent start of a root lead or lane session, the extension appends the guidance text received at
    hello to the system prompt OMP passes in, unless an identical block is already present. It appends to the
    prompt it receives so other extensions' changes survive. If OMP passes a prompt that is not a list of text
    blocks, nothing is changed. Subagents and ordinary sessions get nothing.
37. The guidance text is owned by the daemon's configuration, at most 4 KiB, and covers: what the role owns; that
    merging, deploying, publishing, spending money and destroying data each need an explicit grant, which a review
    or a nudge to keep going is not; that work under a paused goal stays stopped until the user says to resume;
    that running out of provider quota means stop, save progress and report; and that a message the daemon
    authenticated as the owner's counts as an owner request, limited to what it names.
38. When the daemon pins a model for a lane, the extension compares the actual provider and model on every
    provider request of the lane's main agent and before each agent start. On mismatch it aborts the turn and
    fails the request with a hold reason; it never substitutes a model. Subagents keep their own models.
39. The daemon may update the pin (after a lead-approved model change, rule 30). The comparison is exact on
    provider and model ID, case-insensitive; a thinking-level suffix in the pin is compared against OMP's current
    thinking level.
40. At hello and after every model change the extension reports a model readback: requested, observed,
    subscription or metered billing as OMP's model registry reports it, thinking level, and time. If the registry
    is unavailable the status is "unknown", which the daemon treats as a hold (LANE-05).

### Stop gate (EXT-24)

41. When the lead's main agent is about to stop, the extension asks the daemon (timeout 1 s). If the daemon
    returns reminder text, the extension tells OMP to continue with that text as added context. On timeout or no
    answer, the stop proceeds: this gate is a convenience, not a safety rule. Lanes and subagents are never gated.

### Owner questions (EXT-30)

42. For a session the daemon has routed to a messenger, when the agent calls OMP's interactive form tool and no
    tool approval is pending, the extension sends the form to the daemon before OMP opens its local dialog and,
    if the daemon accepts it, completes the form with the answer the daemon returns. The local dialog stays
    usable; whichever answers first wins and the other is withdrawn. Headless sessions use rpc dialogs instead.
    All other rules are in DISC-OUT-15 to DISC-OUT-19.

### Configuration and status (EXT-33, EXT-36)

43. The extension needs one setting: the daemon socket path, taken from a single launch flag or, absent that, the
    per-user default runtime location. It reads no Cataly configuration file and no state directory.
44. One status bar slot shows a short text the daemon may set (for example pending deliveries). On connection
    loss the extension shows one warning and the status "Cataly offline"; it clears both on reconnect.

### Minimal extension: what must remain

The extension's whole job, in order of necessity:

1. Recognise a Cataly session (launch token or session marker) and stay inert otherwise.
2. Hold one client connection to the daemon with hello, heartbeat, reconnect and bounded buffering.
3. Forward OMP events and snapshots (rules 11 to 14).
4. Register two forwarding tools and apply the daemon's visibility rule.
5. Compute tool-result fingerprints and background-job completion facts from the live branch.
6. Execute daemon commands: deliver text, interrupt, set model, shut down; report message-end entry IDs.
7. Hooks that only exist inside OMP: shell-call veto, subagent-spawn veto, system prompt append, provider-request
   model barrier, stop continuation, form interception.
8. Append the session marker and take ownership of a resumed transcript before announcing it.

It holds no lane records, no messenger credentials, no files of its own, no timers other than heartbeat and
reconnect, and no child processes. Target size: under 600 lines of TypeScript. Every rule (who may report, what
counts as evidence, which shell commands pass, which model is correct, what the lead owes) is decided in Rust.

## Third-party interfaces used

- OMP extension API: flag registration and reading; tool registration with inactive default and active-tool
  setting; events session_start, session_switch (with its "new" reason), session_branch, session_tree,
  session_before_switch, session_before_branch, session_before_tree, session_shutdown, goal_updated,
  agent_start, agent_end, turn_start, turn_end, message_end, tool_call, tool_execution_start,
  tool_execution_update, tool_execution_end, tool_approval_requested, tool_approval_resolved,
  plan_review_requested, plan_review_resolved, before_agent_start (system prompt replacement),
  before_provider_request, before_subagent_spawn, session_stop (continue with added context);
  sendMessage (trigger turn, aside, next turn), sendUserMessage (steer, aside), appendEntry, setModel, thinking
  level read, abort, shutdown, idle and pending-message checks, async job snapshot, session manager (ID, file,
  branch, header with parent session, flush to disk), model registry (resolve, available models, subscription
  check), ui notify and status.
- OMP session file format: custom entries and custom message entries with hidden details; reset boundaries.
- OMP rpc-ui protocol for headless sessions (dialogs, process lifetime); see question 1.
- A local stream socket (Unix domain socket; named pipe on Windows) with newline-delimited JSON.

## State that must survive a restart

- In the session file: the Cataly session marker (one small entry per branch) and the transcript-ownership entry
  of rule 7. Nothing else is written by the extension.
- Everything else (role per session, lane binding, shell rules, pinned model, deliveries, evidence) lives in the
  daemon. The extension keeps no state across process restarts; on start it re-sends a snapshot.

## Problems in the old approach, and better ideas

- **The extension was a second daemon.** It ran background commands with restart and backoff, wrote batching
  spill files, polled an inbox database every 3 s, could open its own Discord connection, served a control socket,
  and wrote a health file every second. All of that dies with the OMP process and is duplicated on the Rust side.
  Better: the daemon owns all of it; the extension only forwards.
- **Logic duplicated across languages.** Report acceptance, lane registry reads, shell rule checks, model
  matching and receipt search existed in TypeScript next to Rust or CLI versions, with byte-level parity concerns.
  Better: one implementation in Rust, called over the connection.
- **Files as a message bus.** Session state, health, background-command mirrors and ASK routes were files read by
  other processes, which then needed pid probes, start-time parsing through ps, exit markers and inode checks to
  tell live writers from stale ones. Better: a live connection is the proof of life; its loss is the death signal.
- **Inherited environment as identity.** A terminal block variable inherited by every child made nested OMP runs
  steal and delete the lead's record, which made the watchdog restart the lead repeatedly. Better: a single-use
  launch token checked by the daemon.
- **A multi-step hand-off exchange between a child process and the extension**, with per-message serialisation,
  a quarantine for unclear outcomes and a backwards file scan inside the extension. Better: the daemon tags the
  delivery, the extension reports the entry ID, the daemon checks the file.
- **Special cases for one model family and for a terminal resume pausing a goal.** Better: no model-specific
  prompt patches; the daemon resumes sessions itself, so the pause race cannot occur.
- **Approval handlers cost every OMP process.** Registering them changes OMP behaviour process-wide, which the old
  code worked around by registering lazily. Keep that rule (requirement 1) and measure start-up cost of the
  extension in an ordinary session; target under 5 ms added.

## Questions for the owner

1. For headless sessions, should events and control (deliver, interrupt, model, shutdown) go through the
   extension connection, giving one path for both runtimes, with rpc used only for launch, dialogs and process
   exit? Or through rpc, leaving the extension connection for tern sessions plus hooks only? This spec assumes the
   first; lane-lifecycle currently assumes the second.
2. Should mirroring OMP's form to the messenger work in tern sessions at all, or only in headless sessions where
   rpc dialogs reach the daemon without any interception (which would delete requirement 42)?
3. May the goal tool handling be dropped entirely, relying on the daemon to set every lane's goal at launch and
   reopen, or do lanes need to create their own goal from the brief?
4. Is the restricted second lead-type session (EXT-05) still used? This spec drops it.
5. Should a lane's model stay pinned for its whole life (this spec), or only until its first goal turn as before?
