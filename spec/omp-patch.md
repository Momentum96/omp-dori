# OMP patch removal: behaviour spec

## Purpose

The old product did not run the OMP that users install. It fetched one exact OMP release (19.0.1) from source,
applied its own source patch of about 4,200 lines (85 hunks in 19 files), checked the patched tree byte by byte,
built it, cached it, and refused to start a headless session on any other OMP version. A scheduled job tried to
re-apply the patch to each new OMP release and opened draft changes for review.

The new product carries no patch. This spec lists every behaviour the patch added or changed, says which
product feature needed it, and gives the replacement: a documented OMP surface (REPLACE-PUBLIC), behaviour moved
into the daemon or the small extension (MOVE-OUT), a bug report or change proposed to OMP's maintainers while the
product carries nothing (UPSTREAM), or removal (DROP). It also covers the machinery that existed only to build,
pin, cache and update the patched OMP.

Of the 85 hunks, 41 are OMP test code, about 30 are message-delivery bookkeeping inside the agent session, and the
rest are small fixes to four built-in tools and one web renderer. None of them adds an API that the product calls.
They are generic bug fixes that the old team believed its features exposed.

Stock state, checked read-only against the installed OMP 19.1.1 binary: the todo, eval and message-delivery fixes
are not in stock OMP. The grep and workspace-tree fixes could not be confirmed either way.

## Capabilities

Ranked by risk to the product if the behaviour is lost without a replacement (highest first).

| ID | Capability | Used by | Verdict | Risk | Reason |
| --- | --- | --- | --- | --- | --- |
| PATCH-01 | A message the product injects lands only in the transcript it was meant for, across new, clear, switch, reload and rolled-back switch | monitor wakes, messenger intake, lane replies, startup notices | MOVE-OUT | High | The daemon can serialise its own transitions and confirm delivery from the session file; no OMP change needed for headless sessions. |
| PATCH-09 | Refuse to run against an incompatible OMP | every session start | REPLACE-PUBLIC | High | A minimum version plus the rpc-ui ready frame's protocol fields replace the exact pin and the per-start version probe. |
| PATCH-02 | Eval kernels of a departed session are released only after the session change settles; kernels started for the new session survive | lanes using the Python or JS eval tool | UPSTREAM | Medium | OMP-internal lifecycle; no public hook. Headless sessions avoid the path by never switching in-process. |
| PATCH-05 | Todo done, drop and remove refuse an empty task list instead of hitting a whole phase; remove uses the same target rules as done and drop | every agent that keeps a todo list | UPSTREAM | Medium | Real data-loss bug in a built-in tool; the product cannot wrap a built-in tool without duplicating it. |
| PATCH-12 | Regression evidence that OMP still behaves as the product needs | release gate | REPLACE-PUBLIC | Medium | Replace the patched-source test suite with a contract suite against stock OMP's public surfaces. |
| PATCH-04 | Grep with a line-range selector reports "more results" only when results were really cut | every agent | UPSTREAM | Low | False truncation hint wastes agent calls; generic tool bug. |
| PATCH-13 | Diagnostics report which OMP is in use | health check | REPLACE-PUBLIC | Low | Report executable path, version and protocol from public output instead of a patch digest. |
| PATCH-03 | Workspace-tree snapshot is retaken after a fresh transcript even if tree rendering was off at that moment | none in the product | DROP | Low | Only matters when a user toggles a setting mid-conversation; no product feature depends on it. |
| PATCH-06 | Web viewer and HTML export label todo calls that target several tasks | none in the product | DROP | Very low | Cosmetic, in surfaces the product does not use. |
| PATCH-07 | Extra wait-tool tests (wait cancellation, message interruption, back-off reset) | none at run time | DROP | None | Test-only; the one property the product needs moves into PATCH-12. |
| PATCH-08 | Build OMP from an exact upstream commit plus patch, verify the resulting tree and file hashes, private package cache, export-asset check | install, update | DROP | None | Violates "thin over OMP"; the user's installed OMP is used. |
| PATCH-10 | Content-addressed cache of built OMP trees with leases, pruning, quarantine and retained rollback copies | install, update, rollback | DROP | None | Nothing to cache once OMP is not built by the product. |
| PATCH-11 | Scheduled job that re-applies the patch to each new OMP tag, publishes review drafts or conflict artifacts and issues | maintainers | DROP | None | No patch to carry; new OMP releases are checked by the PATCH-12 suite instead. |
| PATCH-14 | Exact pins of OMP's native add-on and of an OMP host package used only by CI | build, CI | DROP | None | Stock OMP ships its own add-on; the contract suite runs the installed binary. |

Totals: REPLACE-PUBLIC 3, MOVE-OUT 1, UPSTREAM 3, DROP 7.

## Requirements

### Message ownership across session changes (PATCH-01)

What the patch did, in plain words: OMP's extension send-message call can wait internally (for a running context
replacement, for image normalisation, for a hook) before it writes the message. If the session changed during that
wait, stock OMP could drop the message, write it into the new conversation, or lose a passive notice that was queued
for the next turn when the same session was merely reloaded. The patch gave each conversation a private owner token,
held outside senders until a session change and any rollback had finished, retired queued notices on new and clear,
and carried them over on a reload that changed nothing.

The new product gets the same outcome without touching OMP: it never lets a send overlap a session change it can
see, it confirms every delivery from the session file, and it treats anything unconfirmed as "in doubt" and resolves
it by kind. These rules extend the extension's delivery rules in the extension spec (EXT-20, EXT-26, EXT-31).

1. Every message the product puts in front of an agent is a delivery record in the daemon: delivery ID, target
   session ID and session file captured when the record is created, kind (wake, passive notice, steer), text,
   creation time and state (pending, sent, delivered, in doubt, superseded, misdelivered, expired).
2. A delivery may only be written into its target session. The daemon never re-targets a record to another
   session; if the target is gone, a new record is created only where rule 14 says so.
3. Deliveries carry text only. Images and attachments are passed as file paths inside the text, so OMP's
   image-normalisation wait never sits between the call and the write.
4. Headless sessions: the daemon is the only party that can start a session change (new session, switch, branch,
   fork, tree move, clear, compaction with handoff). It keeps one queue per session for deliveries and session
   changes together. A session change is not issued while a delivery for that session is in state "sent", and no
   delivery is sent while a session change for that session is pending. A sent delivery that has not resolved
   within 30 s is moved to "in doubt" so the queue is not blocked longer than that.
5. Headless sessions do not switch to another session inside a running OMP process. When the product needs a
   different session for a lane, it ends the process and starts a new one on that session file. This also makes
   PATCH-02's kernel bug unreachable for headless lanes.
6. Interactive sessions (terminal runtime): the user can start a session change at any time. The extension marks
   the session "changing" when it sees session_before_switch, session_before_branch or session_before_tree, and
   clears the mark on the matching session_switch, session_branch or session_tree event. Because another handler
   may cancel the change and then no after-event comes, the mark also clears after 10 s, at which point the
   extension re-reads the current session ID from the handler context and reports it to the daemon.
7. While a session is marked "changing", or its current session ID differs from the record's target, the extension
   refuses the send as "not current" without calling OMP. The daemon keeps the record pending.
8. A delivery is "delivered" only when the daemon finds a custom-message entry carrying the delivery ID in the
   target session file. The extension's message-end receipt only tells the daemon where to look.
9. If the receipt names a different session file than the target, or the daemon finds the delivery ID in a
   different session's file, the record becomes "misdelivered". The daemon does not resend it, writes one log line
   with both session IDs (not the text), and tells the owner once through the usual alert channel, because text from
   one conversation reached another.
10. When a "sent" record has no receipt within 30 s, it becomes "in doubt". The daemon then searches the target
    session file for the delivery ID (bounded to the last 8 MiB of the file and a 2 s budget). Found: delivered.
    Not found: handled by rule 11 (same session still current) or rule 13 (it is not).
11. Resend policy for an unconfirmed record whose target is still the current session: resend once with the same
    delivery ID (the extension's per-ID memory makes a duplicate call a no-op). A second failure marks the record
    "failed" and alerts the owner for wakes that carry human messages; passive notices are only logged.
12. Reload: when a session event reports the same session ID and file as before (resume or reload with nothing
    changed), every record for that session that was sent but not yet found in the file is resent once. This
    replaces the patch's carry-over of queued passive notices on an unchanged reload.
13. Session left: when the current session ID changes away from a record's target, the record stays pending for
    that target. If the target becomes current again within 24 h, it is delivered then; otherwise it expires with a
    log line.
14. Clear: OMP's session_clear event keeps the session ID but starts a fresh transcript. Records for that session
    that were sent but not found in the file before the event become "superseded". Wakes that carry an unanswered
    human message are re-created once as new records after the clear; passive notices (monitor status, routine
    acknowledgements) are not re-created. Records found in the file before the clear count as delivered.
15. Abort: if the user aborts while a wake is waiting to start a turn, stock OMP keeps the message but starts no
    turn. The daemon accepts that as delivered and does not try to re-trigger a turn for it.
16. Restart of the daemon: pending, sent and in-doubt records survive. On start, every sent or in-doubt record is
    checked against its session file (rule 10) before anything new is sent to that session.
17. All of the above is ordering inside one session. Deliveries to different sessions never wait for each other.

### OMP compatibility (PATCH-09)

18. The configured OMP executable is checked with `omp --version` once at daemon start and again only when the
    executable's path, size or modification time changes. Limit 5 s, output cap 4 KiB. The result is cached in
    memory; session starts do not spawn a version probe. (Same rule as HOST-03 in the session-host spec.)
19. The version must parse and be at or above the configured minimum. The default minimum is the oldest stock
    release the contract suite (PATCH-12) last passed on. Below the minimum: no session starts, health reports the
    found and required versions. A higher major than the newest passed release is allowed but reported as
    "untested" in health; there is no upper pin.
20. Each rpc-ui child must send a ready frame within 15 s. The daemon requires protocol version 1 to be offered and
    negotiates version 2 when offered. Optional features (for example the bash exclude-from-context flag) are used
    only when the ready frame advertises them.
21. The extension checks at load that every OMP extension call and event it uses exists, and refuses to register
    (telling the daemon why) if one is missing. It does not pin an exact OMP package version.

### Eval kernel release (PATCH-02)

What the patch fixed: OMP keeps Python and JS eval kernels per session and releases the kernels of a departed
session once eval work is idle. Stock OMP does not also wait for a session change to finish, so a switch that is
rolled back, or a hook that starts a kernel for the new session, can lose the wrong kernel or leave one running.

22. The product carries no fix. The upstream report describes: after a session change, kernels of a departed session
    are released only when no eval cell is running and no session change is in progress; the kernels of whichever
    session ends up current (including one restored by rollback, and one started by a session hook) are kept;
    shutting the session down still disposes everything without deadlock.
23. Interim: rule 5 keeps headless lanes off the affected path. Interactive sessions keep stock behaviour.

### Todo targets (PATCH-05)

What the patch fixed: an empty task list given to done, drop or remove, together with a phase, acted on the whole
phase; remove resolved its targets differently from done and drop.

24. The product carries no fix. The upstream report describes: done, drop and remove with an empty task list and no
    item list are rejected with an error and no state change; an empty string inside the list is rejected; remove
    resolves targets exactly as done and drop do (named tasks, item list, or phase); any error leaves the whole list
    unchanged; start still needs exactly one task.
25. The product's lane briefing does not tell agents to send empty task lists. No wrapper tool is added.

### Grep completeness (PATCH-04)

What the patch fixed: grep with a path plus a line range fetches a capped number of matches per file. OMP marks the
result "limit reached" whenever a file hit the cap, even when every requested range was fully covered, so agents
paginate for results that do not exist.

26. The product carries no fix. The upstream report describes: the truncation flag is set only when a file reached
    the per-file cap and at least one requested range for that file is open-ended or ends after the last fetched
    match; total-result caps and multi-line matching keep their current meaning.

### Contract suite against stock OMP (PATCH-12, absorbs PATCH-07)

27. A test suite runs the installed stock OMP (no source checkout) in an isolated profile and session directory,
    with a local fake model provider, and asserts only documented behaviour the product depends on:
    a. rpc-ui ready frame, protocol negotiation, prompt, steer, abort, get_state, new_session, switch_session and
       their responses and events;
    b. extension load with the product's extension, session events (start, switch with reason, clear, branch, tree,
       shutdown) and their order relative to rpc commands;
    c. a custom message sent with trigger and aside starts a turn when idle, is persisted as a custom-message entry
       with its hidden details, and ends a running interruptible wait;
    d. a passive next-turn message does not start a turn;
    e. rules 4, 6-9, 12 and 14 of this spec, driven through real session changes.
28. Each case has a 30 s limit; the whole suite 10 minutes. It runs in CI on linux-x64 and darwin-arm64 against the
    minimum supported OMP and the newest stable OMP, and on demand when a new OMP release appears.
29. A failure against a new OMP release changes nothing automatically: it is reported, and the minimum or "tested
    up to" values in rule 19 change only by a reviewed edit.

### Diagnostics (PATCH-13)

30. The health report shows: OMP executable path, version string, whether it meets the minimum, whether it is newer
    than the newest tested release, and the protocol versions from the last ready frame. It shows no patch, tree
    or source identity.

## Third-party interfaces used

- OMP CLI: `--version`, `--mode rpc-ui`, `--no-ui`, `--profile`, `--session-dir`, `--config`, `-e/--extension`,
  `--resume`, `--no-session`, `--approval-mode`.
- OMP rpc-ui: ready frame (`protocolVersion`, `supportedProtocolVersions`, frame limits, feature flags),
  `negotiate_protocol`, `prompt`, `steer`, `follow_up`, `abort`, `get_state`, `new_session`, `switch_session`,
  `get_entries`; events `agent_start`, `agent_end`, `message_end`, `prompt_result`, `session_settled`. Ordinary
  commands are processed in order on one queue; a successful session change cancels earlier input not yet admitted.
- OMP extension API: `pi.sendMessage` with `triggerTurn` and `deliverAs` (`aside`, `nextTurn`), `pi.sendUserMessage`
  with `deliverAs: "steer"`, custom-message `details` and `excludeFromContext`; events `session_start`,
  `session_before_switch`/`session_switch`, `session_clear`, `session_before_branch`/`session_branch`,
  `session_before_tree`/`session_tree`, `session_shutdown`, `message_end`; `ctx.sessionManager.getSessionId()`.
  OMP documents that extension calls are not ordered against `session_*` events, which is why rule 6 has a timer
  and rule 8 checks the file.
- OMP session files: JSONL entries of type `custom_message` with `details`; header with session ID and cwd.
- OMP built-in tools (eval, todo, grep, wait): documented behaviour only.

## State that must survive a restart

- Delivery records (rule 1) in every state other than delivered, expired or superseded, plus delivered ones for
  24 h so a late receipt or duplicate can be matched.
- For each session the daemon drives: session ID and session file last confirmed.
- Configured minimum OMP version and the newest version the contract suite passed on.
- Not needed: the version probe result (re-run at start), any OMP build, cache or lease.

## Problems in the old approach, and better ideas

- A private fork in all but name. Every OMP release needed a three-way re-apply, conflict review, regenerated
  hashes and a full native test run before users could upgrade. Users could not use the OMP they installed. Better:
  run stock OMP, test the contract, report bugs upstream.
- Most of the patch fixed OMP's own bugs, many of them unrelated to the product (web viewer labels, workspace tree,
  grep paging). Carrying them made the product responsible for OMP's correctness. Better: report them, carry none.
- The message-ownership fix tried to make OMP itself exactly-once across every session change, including nested
  hooks and rollbacks, in about 700 lines of concurrency code. The product only needs two things: never send during
  a change it controls, and confirm from the file. That is far smaller and testable from outside.
- Every headless start spawned an extra OMP process just to read its version and compared it to one exact string.
  Slower starts and forced lock-step upgrades. Better: one cached check and a minimum version.
- Building OMP from source pulled a whole monorepo, a package install, an add-on pin, a private package cache, a
  hash-verified result tree, cache leases, pruning, quarantine and rollback copies: well over 3,000 lines of
  product code plus CI to manage someone else's program. All of it goes.
- An automated job opened update drafts and conflict issues against the product's repository. With no patch, new
  OMP releases need only the contract suite.
- Interactive sessions can still race with a user's own session change in the gap between the extension's call
  and OMP's write. The new design detects that case (misdelivered or in doubt) instead of preventing it, and should
  propose to OMP an option on the send call that names the expected session ID and fails if it no longer matches.
  That one small upstream change would close the gap completely.

## Questions for the owner

1. Interactive sessions: is "detect and report" acceptable for a message that lands in the wrong conversation
   during a user's own session change (rule 9), or must the product refuse to deliver into interactive sessions
   whenever a change might be in progress, at the cost of slower wakes?
2. Should the product's maintainers file the four upstream reports (message ownership with an expected-session
   option, eval kernels, todo empty targets, grep completeness) with OMP, or simply drop them?
3. Which OMP version is the minimum at first release (installed today: 19.1.1), and should a newer untested major be
   allowed with a warning (rule 19) or refused until the contract suite has passed on it?
