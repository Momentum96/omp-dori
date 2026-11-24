# Evidence and reporting: behaviour spec

## Purpose

Lanes do the work; the lead and the owner need to know what happened without reading transcripts. This area covers
how a lane tells the daemon what it did and what that claim rests on, how the daemon decides whether a claim shows
real change, how a report reaches the lead and the messenger thread, the few durable records the lead keeps so that
a restart or a context compaction loses nothing (owner requests, questions put to the owner, learned rules, a lane's
completed steps, published files), the read-only status view and its chat copy, the checker that decides whether a
completion condition holds, and the delivery of a message from the lead to a lane.

Users: lane main sessions (report, attach evidence, record steps), the lead session (reads, relays, keeps the
records, messages lanes), the owner (answers questions, reads the status message), and the daemon (single writer of
every record below, enforcer of every rule). The supervision area consumes the last-progress mark defined here; the
lane lifecycle area consumes the completion checker, the step records and the published-file list.

## Capabilities

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| EVR-01 | Lane report with a kind and text, stored durably | lanes, lead | KEEP | The single channel from a lane back to the lead; small and testable. |
| EVR-02 | Lane progress state derived from the latest report | status, supervision | MERGE into EVR-01 | A pure function of the report record. |
| EVR-03 | Activity and stall guesses from terminal screen text | supervision | DROP | Screen scraping; OMP agent events give the same facts exactly. |
| EVR-04 | Fixed labelled layout for a lane's final report | lanes | MERGE into LANE-02 | Prompt guidance only; the daemon never parses it. |
| EVR-05 | Short owner-facing summary beside the detailed text | thread relay | SIMPLIFY | One optional field; no separate posting mode switch per lane. |
| EVR-06 | File evidence (content hash of a named file) | lanes | SIMPLIFY | Keep hash plus stability check; drop the timestamp-race machinery. |
| EVR-07 | Tool-call evidence (fingerprint of a completed tool result) | lanes | SIMPLIFY | The extension sees results directly; keep a short refusal list. |
| EVR-08 | Evidence novelty and the last-progress mark | supervision | KEEP | The only honest progress measure; cheap to compute. |
| EVR-09 | Ordering numbers shared across two journals | last-progress mark | MERGE into EVR-08 | A single writer orders acceptances by itself. |
| EVR-10 | Passive scan of changed files (git status plus stat marks, 256 a cycle) | supervision | DROP | Polling the disk; tool events from OMP show writes as they happen. |
| EVR-11 | Binding a report to the lane run and session that wrote it | daemon | SIMPLIFY | The extension connection already identifies the run; no environment variables. |
| EVR-12 | Report receipt with a stable ID | lanes, lead | KEEP | Needed for dedupe, relay linking and completion claims. |
| EVR-13 | Lane pastes its own report line into the lead's input | lanes | DROP | Duplicates a daemon duty, needs terminal checks, and can be lost or doubled. |
| EVR-14 | Report relay to the lane's messenger thread | owner | SIMPLIFY | Daemon posts directly; drop the shell hook and two-pass screen hold. |
| EVR-15 | Lead acknowledges a manual relay and links a reply receipt | lead | MERGE into EVR-14 | Automatic when the lead replies through the daemon. |
| EVR-16 | A stuck report names what it waits for (CI, label, session) | supervision | MERGE into the supervision area | Wait tracking is not reporting. |
| EVR-17 | Published files: configured roots, filters, versions, public link | owner | SIMPLIFY | Keep register, version and link; drop delivery claims and noise fields. |
| EVR-18 | Operator-reported delivery receipts and step cross-links for files | lead | DROP | Self-asserted data that proves nothing. |
| EVR-19 | Keep a worktree that holds a published file at close | close | MERGE into LANE-24 | One more condition in the worktree removal rule. |
| EVR-20 | Step records with remote-effect receipts, idempotent by step | lanes | KEEP | Stops repeated pushes, merges and deploys after a crash or reopen. |
| EVR-21 | Track pull-request links found in step records and claims | PR follow-up | MERGE into the PR follow-up area | Same link extraction, owned there. |
| EVR-22 | Owner question ledger (ask, bind message, answer, withdraw, digest) | lead, owner | SIMPLIFY | Keep the record; single writer removes the readback protocol. |
| EVR-23 | Matching an owner reply to its open question | lead | SIMPLIFY | The daemon matches on intake instead of the lead running a lookup. |
| EVR-24 | Lead chooses among named options of a lead-decidable question | lead | KEEP | Saves the owner from trivia while keeping a written reason. |
| EVR-25 | Request log of owner requests (open, next step, notes, close) | lead | SIMPLIFY | Keep the record; the boot summary reads it. |
| EVR-26 | Generated Markdown copy of the request log | operator | DROP | A second file to guard; the summary and listing cover it. |
| EVR-27 | Learned rules handed to new lanes (repo and global) | lead, lanes | SIMPLIFY | Two states instead of five; authority by session, not environment. |
| EVR-28 | Recovery summary after restart or compaction | lead | SIMPLIFY | Pushed by the extension on the right event; no command to remember. |
| EVR-29 | Read-only status view | lead, voice, owner | KEEP | One call answers "what is going on". |
| EVR-30 | Status message kept current in a chat | owner | SIMPLIFY | Update on change inside the daemon instead of a timed loop process. |
| EVR-31 | Duration estimate from past lanes with a key prefix | lead | DROP | Rarely right, never acted on. |
| EVR-32 | Completion-signal checker | lifecycle, queue | SIMPLIFY | Structured input; drop the package-registry kind (a command covers it). |
| EVR-33 | Lead-to-lane message delivery with an in-doubt fence | lead | SIMPLIFY | rpc answers remove most doubt; serialise in memory, not with lock files. |
| EVR-34 | Short notices to a lane about its claim or an objection | lifecycle | MERGE into EVR-33 | Same path with no waiting. |
| EVR-35 | Proof that an inbound item reached the lead's transcript | intake | MERGE into DISC-IN-15 | Already specified as the hand-off confirmation. |
| EVR-36 | Owner requests not yet answered, and the reminder before the lead stops | lead | MERGE into DISC-OUT-09 | Specified in the outbound area. |
| EVR-37 | Extra prompt block for one model family about batching bookkeeping | lanes | DROP | Model-specific prompt patch; one tool call replaces the shell chain. |
| EVR-38 | Compare lane records with live sessions and store found session IDs | lead | MERGE into LANE-27 | Same as the drift report. |
| EVR-39 | Pull-request policy check (commit identity, fixes need tests) | lanes | MERGE into EVR-32 | An optional signal kind; drop the UI test loop heuristic. |
| EVR-40 | Thread title "finished" mark and last-lane-in-thread election | lifecycle | MERGE into LANE-16 | Thread binding owns it. |
| EVR-41 | File locks with owner records, stale takeover, readback nonces, torn-line repair, stat caches | all records | DROP | One daemon writer and one embedded store make all of it unnecessary. |

## Requirements

### Reports from a lane (EVR-01, EVR-02, EVR-05, EVR-11, EVR-12)

1. A lane reports through one tool that the Cataly OMP extension registers for the lane's main agent only.
   Subagents, the lead and unregistered sessions do not get it. The tool forwards the call to the daemon over the
   extension's connection and returns the daemon's answer.
2. Inputs: kind, one of progress, stuck, waiting-on-owner, stopped, finished; text, 1 to 8000 characters after
   trimming; optional owner summary, 1 to 500 characters; optional evidence lists (files, tool-call IDs; see 9-16).
   Empty text, an unknown kind or an oversize field is refused and nothing is stored.
3. Authority: the daemon accepts a report only when the calling connection belongs to the current run of an open
   lane and to the OMP session recorded for that run. A report from an earlier run, a closed lane, or a session the
   lane no longer owns is refused with "the lane moved on". The lane never names its own key; the daemon knows it.
4. The daemon assigns a per-lane increasing report number, the acceptance time from its own clock, and the run
   identity, writes the record durably, and only then answers. The answer carries the report ID (lane key plus
   number), the time, and one line on the attached items: "nothing attached", "all attached items were seen
   before", or "N of M attached items count as new".
5. If the write fails, the tool returns an error, nothing is relayed, and a retry is safe because nothing was stored.
6. The stored text is kept exactly. Any display of it (lead wake line, thread post, status) strips terminal escape
   sequences and control characters and folds line breaks into single spaces.
7. Lane progress state is derived from the newest report of the current run: progress gives moving; stuck and
   waiting-on-owner give waiting (waiting-on-owner lanes are never nudged); stopped gives stopped; finished gives
   finished. A lane with no report yet is moving. Reopening a lane starts a new run whose state starts as moving with
   no latest report.
8. Activity facts from OMP (agent turn started or ended, auto-compaction failed) may switch a moving lane to quiet
   and back, as the supervision area decides. They never replace waiting, stopped or finished.

### Evidence (EVR-06, EVR-07, EVR-08)

9. File evidence: each path is resolved against the lane session's working directory, then to its canonical path
   with symlinks resolved. It must be a regular file of at most 1 GiB. The daemon hashes its bytes with SHA-256 and
   compares device, inode, size and modification time before and after reading. A difference refuses the report
   with "file was being modified; retry after it settles". A missing path, a non-regular file, a symlink loop and an
   unreadable file are each refused with their own reason.
10. Tool evidence: each ID must name a tool call on the session's current branch whose result is present, not an
    error, and not pruned, and whose result belongs to the same tool. The extension, not the lane, reads the branch.
11. When a tool call only launched background work, it counts only if the extension saw OMP report that work as completed
    successfully; the extension then records the completed result's fingerprint for the call. A job still running,
    failed or cancelled is refused.
12. Refused because they organise the lane rather than change anything: the report tool; messaging and question
    tools; todo, goal and thinking tools; any call whose target is an agent or ask address; any shell or eval call
    that runs the product's own command line tool or whose output contains a report wake line. Also refused: any
    eval result in which a cell did not finish with success, and, for a launched run, any call the extension did not
    see start in this run.
13. Tool fingerprint: SHA-256 over the tool name, the visible result content (including images) and the structured
    result details, excluding run timing, job bookkeeping and output-file locations. A rerun with an identical
    result has the same fingerprint.
14. Evidence is all or nothing: one refused item refuses the whole report, naming the item and the reason.
15. Novelty: a file item is new when its hash differs from the last hash recorded for that canonical path in this
    run, or the path was never recorded. A tool item is new when its fingerprint was never recorded in this run.
    So a lane cannot earn progress by restating a report, saving a file with identical bytes, or repeating a call
    that yields an identical result.
16. The last-progress mark of a run is the acceptance time and identity of the latest new item, over reports and
    step records together, in daemon acceptance order. Supervision reads it; nothing else moves it.

### Getting a report to the lead and the thread (EVR-13, EVR-14, EVR-15)

17. On acceptance the daemon delivers one wake line to the lead: lane key, kind, report ID and the first 500
    characters of the display text, plus "(truncated)" when cut. Stuck, waiting-on-owner, stopped and finished start
    a lead turn; progress is queued for the lead's next turn without starting one.
18. The lane never sends anything into the lead's input itself.
19. Thread relay: when the lane has a messenger thread, the daemon posts the report there unless policy holds it.
    Policy, set in config: which kinds post automatically (default all) and whether raw text may be posted (default
    yes). If raw text may not be posted, only reports with an owner summary post automatically and the summary is used.
20. A thread post reads "Lane update:" followed by the summary or display text, with the home directory
    shown as "~", any other absolute local path replaced by "[path removed]", web links reduced to origin and path,
    and mentions disabled. Each report ID is posted at most once.
21. A held report is announced to the lead once with the reason (kind not auto-posted, no summary, no thread).
22. A post rejected by the platform is retried at most 3 times with growing delay (30 s, 2 min, 10 min); after that
    the lead is told once. A post whose outcome is uncertain is never retried; the lead is told once.
23. When the lead itself replies in the thread through the daemon and names a report ID, the daemon marks that
    report relayed, links the outbound reply receipt (outbound area), and stops any pending automatic post for it.
24. Each report shows one relay state: not posted, held (reason), posted, posted by the lead with receipt, failed.

### Published files (EVR-17)

25. The owner may configure publish roots: an absolute directory and an optional public base URL (http or https,
    no credentials, query or fragment). Without roots the feature is off and every request is refused.
26. A lane (for itself) or the lead (for any open lane) registers a file with an optional title (1 to 200
    characters, default the file name) and a visibility, private or public. Refused when: the lane is not open; the
    file is not inside a root (longest root wins) before and after resolving symlinks; any path part below the root
    starts with a dot, contains "secret", "credential", "token" or "private" in any case, or is a dependency folder;
    the extension is not on the allow list (web page, style, script, JSON, common image, font, Markdown, text, CSV,
    PDF, common audio and video); it is not a regular file; it changed while hashed; public is asked for a root
    without a URL; or the file lies inside a worktree of another open lane.
27. A registration stores SHA-256, size, canonical path, title, visibility, link (public only, built from the base
    URL and the percent-encoded relative path) and time. Registering an unchanged file again adds nothing; a change
    in bytes, title or visibility adds a version.
28. Listing shows each file with its latest version and a live state: current (bytes and link match), changed, or
    unavailable (with reason). Nothing is uploaded, fetched or marked as viewed.
29. A finished report of a lane with public files gets one line per current public file appended (title, link,
    hash); files not current are listed as withheld; private files are only counted.

### Step records (EVR-20)

30. A lane records a completed step with: a step name unique within the lane (1 to 200 characters), a result
    (1 to 1000), an evidence reference (a link or path, 1 to 1000), zero or more remote-effect receipts (push commit,
    merge link, deploy ID; each 1 to 500), and optional file and tool evidence under rules 9-14.
31. The same step name with the same result, evidence and receipts returns the original record, marked unchanged;
    evidence is not read again. The same name with different content is refused as a conflicting step. A receipt
    already recorded under another step is refused, naming that step.
32. A step record is durable before the answer. It carries the run identity; if the run cannot be established it is
    still stored, marked "run unverified", because losing a receipt is worse.
33. When a lane is reopened or resumed, its instruction block lists every recorded step of the lane (all runs),
    newest last, and the lane can list them again with a tool. The instructions tell the lane that effects already
    listed must not be done again, and that an effect whose record may be missing because of a crash must be
    confirmed by looking at the remote system before anything is retried.

### Owner questions (EVR-22, EVR-23, EVR-24)

34. The lead records a question it puts to the owner: text (one line, 1 to 1000), a source reference (where the need
    came from, 1 to 400), optional lane, thread, 1 to 9 options (each one line, 1 to 200), a recommended option
    number, a request-log item, and the posted message reference. The daemon assigns an 8-hex-digit ID.
35. Who settles: owner-only (the default) or lead-may-choose. Lead-may-choose needs at least 2 options and a thread;
    when a lane with a thread is given, it must be that thread. Cost, access, outside commitments, irreversible work
    and changes of scope stay owner-only; the lead's instructions say so, the daemon cannot judge content.
36. One open question per posted message. Binding a message that another open question holds is refused.
37. When an owner message arrives that replies to a message bound to an open question (Slack: to the bound thread
    root), the daemon records it as the answer before handing the message to the lead, and the hand-off says which
    question it answered. A bare number within the options names that option; a number outside them is not recorded
    and the lead is told; other text is a free answer. Replies to unbound messages are ordinary messages.
38. The lead may also record an owner answer by hand, citing the owner's message reference.
39. The lead may choose for a lead-may-choose question only: an option number in range, a reason (1 to 1000), and the
    reference of the message in the question's own thread that announced the choice. A different thread is refused.
40. Withdraw takes a reason. The first answer, choice or withdrawal settles a question; later attempts are refused
    with the current state.
41. Settling a question linked to an open request-log item appends a note to that item. If adding the note fails,
    the answer stays recorded and the lead is warned.
42. The digest lists open questions, one line each: ID, age in whole hours, lane, who may settle, the question and
    the numbered options with the recommended one marked. When reminding the owner, the lead uses this list rather
    than recalling questions itself.
43. Recording or answering a question has no effect on OMP's tool approvals; those are handled only where OMP asks.

### Request log (EVR-25)

44. The lead adds an item for each owner request: title (one line, 1 to 200), source reference (1 to 400), optional
    thread. It then sets the next step (replaces, 1 to 1000), appends notes (1 to 1000), and closes the item with a
    result (1 to 1000). Changing a closed item is refused. Listing shows open or all items.

### Learned rules (EVR-27)

45. A learned rule has text (one line, 1 to 400), evidence (one line, 1 to 2000), the lane it came from, a scope
    (one repository, by canonical directory, or global), a time and a state, active or retired. Only the state may
    change after creation.
46. Lanes may add and retire rules for their own repository. Only the lead adds global rules or changes their state.
    The daemon decides by the calling connection, not by anything the caller declares.
47. At launch the daemon puts before the brief, separately headed, the newest active global rules (at most 12) and
    the newest active rules for the lane's repository (at most 8). If any eligible rule is left out, the lead is told
    which, at launch.

### Recovery summary (EVR-28)

48. At the start of a lead session and after every compaction of it (OMP extension events), the extension adds a
    recovery summary from the daemon to the lead's context. It holds: each open lane with status, latest report kind
    and time or "no report", held or suspended lanes with reasons; every open request-log item with its next step;
    every open owner question (ID, age, lane, thread, question); the count of open owner obligations; and each
    damaged record by name. It is at most 8 KiB; overflow ends with "and N more" per section.
49. Producing the summary changes nothing. If the daemon cannot be reached, the summary is one line saying so.

### Status view and status message (EVR-29, EVR-30)

50. The status view lists open lanes (or one named lane, open or closed), sorted by thread then key, each with: key,
    lane status, elapsed minutes, agent idle or working, goal state, approval pending, progress state and its time,
    latest report (time, kind, one line), latest step record (time, step, result), open questions for the lane,
    last nudge, last thread post, relay state of the latest report, and warnings. Totals: open questions, scheduled
    watches. Any value that could not be read shows as unknown with a warning; nothing is guessed. It writes nothing.
51. A status message is one message per configured destination (Discord channel or thread, Telegram chat or topic).
    Its body fits in one message with a margin (Discord 2000, Telegram 4096 characters): a header with lane count and
    open questions, then per lane one line (key, status, agent state, approval pending or progress state) and the
    latest report cut to 240 characters, cleaned as in rule 20. Lanes that do not fit are summed as "N more". The
    last line is the update time.
52. The daemon rebuilds it when a lane changes status, a report is accepted or a question changes, at most once per
    60 s per destination. An unchanged body makes no API call.
53. Before creating the message the daemon records "create pending". A confirmed creation stores the message ID; later
    updates edit it. An uncertain creation is never repeated: the destination stays in doubt until the lead supplies
    the message ID it found. A clearly rejected creation (4xx) clears the pending mark. Telegram has no message
    lookup, so its recovery accepts only an ID the daemon recorded earlier.

### Completion signals (EVR-32, EVR-39)

54. A completion condition is a non-empty list of signals given in structured form at launch. Kinds: pull request
    merged (repository and number); issue closed (repository and number); command succeeds (argument list, optional
    output pattern); file check (path, optional SHA-256, optional JSON field path and expected value); web check (URL,
    expected status default 200, optional body pattern); pull request passes policy (repository and number).
55. Launch refuses a condition with no signals or with an invalid signal, naming each one.
56. A check runs all signals in parallel and returns per signal pass or fail and a detail of at most 120 characters.
    The condition holds only if every signal passes. A signal that cannot be evaluated fails with the reason.
57. Pass rules. Merged: GitHub says merged and gives a merge commit. Closed: GitHub state closed. Command: run without
    a shell in the lane's directory, 120 s limit, exit 0 and the pattern (if any) matches standard output. File: path
    relative to the lane directory or home-relative, exists, hash equal and JSON field equal when given. Web: one GET,
    30 s limit, status equal and pattern (if any) matches the body. Every GitHub call is bounded at 120 s.
58. Policy pass: every commit's author and committer email equals the repository's configured identity (case
    insensitive), except that a merge commit's committer may be GitHub's no-reply address; and if any fix-type commit
    changes source files, the pull request adds or changes a test file or its description gives an explicit no-test
    reason. The pull request head, description and commit count are read before and after; any change fails with
    "moved while being checked". More than 3000 files in a commit or the pull request fails. A repository without a
    configured identity fails.

### Message from the lead to a lane (EVR-33, EVR-34)

59. The lead sends text to a lane by key. Refused, with nothing sent, when the lane is closed, a close is in
    progress, the lane is suspended or held, it has no confirmed session, its run changed since the lead last read
    it, or an earlier delivery to it is in doubt (the refusal names that delivery).
60. Deliveries, holds and closes for one lane are serialised in the daemon. A delivery either finishes before a hold
    takes effect or sees the hold and is refused. A caller waits at most 30 s for its turn, then is refused with the
    name of the operation in progress.
61. Before sending, the daemon records the delivery as in doubt. It then submits the text through OMP rpc (headless)
    or the extension connection (tern). Outcomes: confirmed when OMP accepts the prompt; failed when OMP refuses it or
    the session cannot take input (not ready, approval prompt open), recorded and reported with the session state;
    unconfirmed when the connection drops or no answer arrives within 10 s, which leaves it in doubt.
62. An in-doubt delivery blocks further deliveries to that lane until the lead inspects the session and releases it
    with a note. Release only lifts the block; it never resends and never decides what happened.
63. Notices from the lifecycle area (claim recorded, objection) use the same path without waiting; a refused or
    unconfirmed notice is reported to the lead once.

## Third-party interfaces used

- OMP extension API: tool registration for the lane main agent; `tool_execution_start`, `tool_execution_update`
  and `tool_execution_end` with background job state; reading the session's current branch entries; adding custom
  entries; `before_agent_start`, `agent_start`, `agent_end`, `session_start`, `session_compact` and `session_stop`;
  and sending a message that either starts a turn or waits for the next one.
- OMP rpc-ui protocol: submit a prompt to a headless session and read its acceptance answer.
- GitHub through `gh api` (read only): pull request, its commits and files, single commit, issue state.
- Discord REST: create message, edit message, with allowed mentions empty. Telegram Bot API: sendMessage,
  editMessageText with chat and topic IDs.
- Plain HTTP GET for web signals.

## State that must survive a restart

- Per lane and run: every report (number, time, kind, text, summary, evidence items with hashes, relay state and
  receipt link) and every step record (with receipts, evidence and run identity).
- Per run: the set of evidence identities already seen with their last hash, and the last-progress mark.
- Per lane: published files with all versions; the in-doubt delivery, if any.
- The owner question ledger, the request log and the learned rules, with full history.
- Per status-message destination: message ID, last body, and the create-pending or in-doubt mark.
- Pending thread posts with their retry count.

## Problems in the old approach, and better ideas

- A lane wrote its report through a command-line tool, then pasted the resulting line into the lead's terminal after
  checking the lead was idle. That needed the lead's address, idle checks and "never resend" rules, and could still
  be lost. The daemon now wakes the lead itself on acceptance.
- Activity and stalls were judged from screen text (spinner words, prompt characters, compaction error phrases).
  OMP's own agent and compaction events replace all of it.
- Report authorship was proven by replaying an execution journal and an environment variable that lanes were told
  never to touch. The daemon owns the connection of each run, so authorship is known.
- Every record had its own lock file with owner records, two-minute stale takeover, rename-aside release, random
  readback tokens and repair of half-written lines; one record even used an embedded database only as a mutex.
  One daemon process writing one store removes this whole layer.
- A passive watcher listed changed files with git and re-hashed up to 256 per cycle to tell work from silence.
  Tool events already tell the daemon when a lane writes.
- Reports were relayed to threads through a configurable shell hook, with a two-pass hold for screen-read lines.
  The daemon posts through the messenger area directly.
- Questions needed the lead to run a lookup before reading a reply as an answer; the daemon can bind at intake.
- The status message was refreshed by a separate long-running loop process on a timer; it is now updated on change.
- Learned rules had five outcome states, of which only two changed behaviour.
- A Markdown copy of the request log, a duration estimator and operator-asserted delivery receipts added files and
  commands nobody needed to act.
- A prompt block patched one model family's habit of splitting bookkeeping across turns; with reports as one tool
  call the problem does not arise.
- Completion conditions used a home-grown text mini-language parsed with hand-written JSON scanning; structured
  input removes the parser and its error cases.

## Questions for the owner

1. Published files: do you use the public-link feature at all? If not, it can be dropped and the worktree-keeping
   rule with it.
2. Should progress-kind reports wake the lead (start a turn), or only wait for its next turn as proposed?
3. The pull-request policy check (commit identity, fixes need tests): keep it as a completion signal, or drop it?
4. Text caps are new (8000 characters per report, 500 per summary, 1 GiB per evidence file). Are they acceptable?
5. Request log versus OMP's own goals and todos: keep a separate request log for the lead, or rely on OMP's?
