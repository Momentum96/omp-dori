# Cataly design

Status: proposed, revised 2026-11-25 after the owner's decisions in `docs/decisions.md`, which override any
default here (section 12.1). Written on the build side of the clean room from the 13 specs in `spec/` only.
Every decision below names the principle it serves: **thin** over OMP, **light**, **fast**, **stable**
(`docs/principles.md`). Only capabilities marked KEEP, SIMPLIFY or MERGE (and, in `spec/omp-patch.md`,
REPLACE-PUBLIC and MOVE-OUT) are built. `docs/plan.md` assigns each of them to a milestone.

OMP version this design was checked against: **omp 19.1.1** (`omp --version` prints `omp/19.1.1`), the
compiled binary at `~/.bun/bin/omp`. Its bundled documentation (`omp read omp://rpc.md` and the other
`omp://` pages) was read, and a short rpc-ui session was run against it with no model call (section 3.2).
The globally installed `@oh-my-pi/pi-coding-agent` npm package is 17.4.0 and does not match the binary, so
its typings were not used as evidence.

## 1. Glossary

The specs use neutral words. Cataly uses the words below, and only these, in code, CLI, logs and messages.

| Cataly word | Meaning | Spec wording it replaces |
| --- | --- | --- |
| Owner | The one person Cataly works for. Identified per relay by a configured account ID. | owner, operator |
| Hub | The one long-lived Rust process (`cataly hub`). Owns every session, connection, job and record. | daemon |
| CLI | Short-lived `cataly <verb>` calls. Each one sends one request to the hub. | command line, local command |
| Bridge | The TypeScript extension file `cataly.ts` that OMP loads. A forwarder, never a decider. | Cataly OMP extension |
| Session | One OMP agent conversation the hub runs or has registered. | OMP session |
| Lead | The standing session that alone talks to the owner. | lead session |
| Lane | A separate session for one piece of work, with brief, directory, model and done checks. | lane, worker |
| Run | One start of a lane (first open or a reopen). Has a random run ID. | run identity |
| Runtime | How a session's OMP process is hosted: `headless` (default) or `tern` (visible Tern block). | runtime, tern path |
| Relay | The adapter for one chat platform: Discord, Telegram or Slack. | messenger, platform |
| Room | A conversation on a relay: channel, DM, group, Slack channel. | channel, chat, conversation |
| Thread | A sub-conversation: Discord thread, Telegram forum topic, Slack thread. | thread, topic, sub-conversation |
| Request | An owner message the hub has authenticated and admitted. Everything else is **data**. | owner message, request |
| Delivery | Text the hub puts in front of a session. A **wake** starts a turn; a **note** waits for the next one. | injection, hand-off, wake line |
| Inbox | The per-target queue of deliveries (one for the lead, one per lane key). | mailbox |
| Receipt | The durable record that an outside effect happened (admission, post, thread, push). | receipt |
| In doubt | An effect that may or may not have happened. Never repeated automatically except where 9.3 says. | in doubt, uncertain |
| Fence | A session or room the hub refuses to send into until someone acts (model mismatch, in-doubt hand-off, misdelivery, repeated spend offers). | held, hold (of a session or conversation) |
| Pause / resume | Stop and continue a lane's work while keeping its process. | hold / continue |
| Park / abandon | Close a lane's process but keep files, with "may come back" or "given up" as outcome. | park / abandon |
| Claim / objection | A lane says it is done; the lead may object within the window. | completion claim, objection |
| Done checks | The machine-checkable conditions given at open (PR merged, command succeeds, ...). | completion condition, signals |
| Report / step | A lane's typed message to the hub / a durable record of a completed external effect. | report, step record |
| Progress mark | Time of the latest new evidence in a run. The only progress measure. | last-progress mark |
| Event | One typed fact in the hub's event stream, **urgent** (starts a lead turn) or **quiet** (waits). | wake line, tagged line, notice |
| Job / job kind | One configured instance / the built-in Rust code that runs it. | watcher, timer, monitor |
| Tick | One run of a job. Compares conditions with **transition memory** and emits events only on change. | pass, sample |
| Quota gate | The hub-wide flag that keeps new session starts waiting while a quota is near its limit. | quota hold |
| Oversight | The per-lane watch: reminders, stall alert, stuck verdict, ceilings. | supervisor, lane loop |
| Planner | The hourly pass that starts backlog items and asks their questions. | planner |
| Backlog item | A piece of later work the planner may start as a lane. | work item |
| Slot | A named, priority-ordered exclusive right to run a heavy command. | heavy-work slot, named slot |
| Question | An owner question recorded by the lead, bound to the posted message. | owner question ledger entry |
| Form / approval / dialog | OMP's `ask` form / a tool-approval prompt / any `extension_ui_request` awaiting an answer. | owner question (OMP), permission request |
| Grant | A recorded owner authorisation naming act and target. | grant |
| Owed reply | An admitted request that has not yet received a linked answer. | obligation |
| Request log | The lead's list of owner requests with next step and result. | request log |
| Board / board post | The read-only status view / its copy kept current in a chat. | status view / status message |
| Recap | The summary put in front of the lead at start and after compaction. | recovery summary, startup summary |
| Digest | The "while you were away" summary built when the owner returns. | away digest |
| Profile | The owner-notes file the lead keeps (likes, topics, decisions). | owner notes, owner profile |
| Probe | The hub's start-up check of the installed OMP's version and features. | compatibility check, health check |
| Call | A voice conversation in a Discord voice channel. | voice session |
| Contract suite | The tests that run stock OMP and assert only what Cataly relies on. | regression suite |

## 2. Process model

### 2.1 What runs where

| Piece | Lifetime | Does |
| --- | --- | --- |
| `cataly hub` | Long-lived, one per OS user. Holds an exclusive OS file lock on `<state>/hub.lock`. | Everything: sessions, relays, voice, jobs, store, control socket. |
| `cataly <verb>` | One request, then exit. | Owner commands, status, setup, install/update. Also `cataly slot` (stays until its wrapped command exits). |
| `cataly.ts` (bridge) | Inside every OMP process. Inert unless the hub accepts the session. | Hooks that only exist inside OMP, tool forwarding, evidence fingerprints, and (tern runtime only) control. |
| OMP children | Started by the hub, one per headless session, over pipes. | The agent. |
| Job children | Bounded subprocesses: `gh`, `git`, `omp usage --json`, `omp session lease --json`, owner-configured argv. | Inputs for jobs. Never resident helpers (OJ requirement 14). |

No other process exists. Supervisor, planner, watchers, presence, intake and the job runner are tasks inside
the hub (**light**: the old product ran a dozen resident processes).

### 2.2 Inside the hub

- One Tokio runtime with two worker threads, plus one dedicated store-writer thread (section 7). Two workers
  are enough for I/O-bound work and keep thread stacks out of the resident set.
- **Actors, not locks.** Each session is an actor that owns its child's stdin/stdout and processes state
  changes one at a time (HOST-21). Its latest snapshot is published on a watch channel, so status reads never
  wait. Each lane is an actor that serialises lifecycle actions (LANE req. 31). Each relay is a task. The scheduler
  is one task holding a min-heap of due times. There are no file locks between parts.
- **Supervision.** Every long-lived task is started by a supervisor that logs its exit reason and restarts it
  with backoff (1 s doubling to 60 s). More than 5 restarts of one task in 10 minutes, or any failure of the
  store-writer thread, makes the hub exit with status 70 so the service manager restarts it cleanly.
- **Liveness.** A stall detector thread watches a counter the main loop advances every second. Under systemd
  the hub sends `WATCHDOG=1` every 10 s only while the counter moves (unit `WatchdogSec=30`); under launchd it
  exits after 60 s without progress (OJ requirement 47). The next start posts one note.

### 2.3 Targets and how they are measured

Targets are budgets for review, not promises; each is measured before anything is optimised (**fast**).

| Measure | Target | How measured |
| --- | --- | --- |
| Hub resident memory, idle, three relays connected, owner jobs configured, no sessions | ≤ 30 MiB | `VmRSS` from `/proc/self/status` (Linux), `ru_maxrss` from `getrusage` (macOS), sampled by `cataly stats` and by the CI benchmark after a scripted scenario. A rise of more than 10 % against `main` fails the job. |
| Added per attached session (rings, buffers, actor), OMP excluded | ≤ 1 MiB | Same benchmark with 8 headless sessions on a fake model. |
| Added by all owner jobs | < 5 MiB | Benchmark with every owner job enabled (OJ requirement 14). |
| Added during a voice call | ≤ 25 MiB | Voice benchmark with recorded audio (milestone 5). |
| Hub start to control socket answering | ≤ 150 ms | Timestamp at process start versus first successful `status` in CI. OMP probe runs after, off this path. |
| Wake, hub part: relay frame read to `prompt` line written to OMP stdin | p50 ≤ 5 ms, p99 ≤ 25 ms. An owner request from a relay to the lead is written while its durable commit runs (decision 15), and on Discord the arrival commit runs beside the hand-off too, so no fsync is on this path there; other deliveries (lane messages, hub notes, `cataly lead send`, Telegram and Slack arrivals) still commit first | The hub stamps each request at frame read, commit, write and `agent_start`; the last 1,000 samples form a histogram served by `cataly stats latency`. |
| Wake, end to end: relay frame read to OMP `agent_start` | p50 ≤ 150 ms, p95 ≤ 300 ms on the CI runner | Contract suite scenario: fake Discord Gateway, stock OMP, loopback fake model. |
| CLI round trip (`cataly status`) | p50 ≤ 20 ms | CI benchmark, 100 calls. |
| Bridge cost in an ordinary (non-Cataly) OMP session | < 5 ms added start-up | Contract suite: OMP start with and without the extension link (EXT problems section). |

## 3. OMP boundary

### 3.1 Rule

Cataly uses only documented public surfaces of stock OMP: CLI flags, rpc-ui commands and frames, the
extension API, settings, environment variables, session files and the `omp usage` / `omp session lease`
commands. No patch, no import of OMP internals, no reading of OMP files other than session files (**thin**).
Every surface below was checked against omp 19.1.1 on 2026-11-25 and carries one of these marks:

- **live**: exercised against the installed binary (section 3.2);
- **docs**: described in the installed binary's own documentation (`omp read omp://<page>`);
- **help**: listed by `omp --help` or a subcommand's `--help`;
- **unverified**: not found in documentation; Cataly must not depend on it until the contract suite proves it.

`live`, `docs` and `help` all count as **verified against omp 19.1.1**; only the last row group is
**unverified**. A surface listed as verified by documentation alone is still covered by a contract-suite case
before the capability that uses it ships (section 10.4).

### 3.2 Live probe used for verification

`omp --mode rpc-ui --no-session --no-extensions --no-skills --no-rules --no-context-files --no-tools --offline
--approval-mode always-ask` with `PI_NOTIFICATIONS=off`, fed `negotiate_protocol` (2), `get_state`,
`goal get`, `set_ask_dialog`, `get_session_lease` and `set_host_tools []`, then end of input. Observed: a
`ready` frame with `protocolVersion 1`, `supportedProtocolVersions [1,2]`, `maxFrameBytes 1048576`,
`maxReassembledFrameBytes 67108864`, `supportsBashExcludeFromContext true`; success responses echoing the
command name for all six commands; `get_state` fields including `model`, `sessionId`, `isStreaming`,
`isCompacting`, `queuedMessageCount`, `isSettled`, `hasPendingAsyncWork`, `goal`; clean exit in 2 s.

### 3.3 Surfaces used

| Surface | Used for | Mark (19.1.1) |
| --- | --- | --- |
| `omp --version` → `omp/X.Y.Z` | Probe, minimum version | live |
| `--mode rpc-ui`, `--approval-mode always-ask`, `--config <overlay>`, `--model provider/id`, `--thinking`, `--session-dir`, `--resume=<path>`, `--append-system-prompt`, `--tools`, `--max-cost`, `--no-session`, `--no-extensions`, `--no-skills`, `--no-rules`, `--no-context-files`, `--no-tools`, `--offline`, `--profile`, `-e` | Launch (section 4), probe, tests | help (rpc-ui, approval-mode, no-* and offline also live) |
| `omp usage --json` | Quota job, limits view | help (JSON schema unverified: parsed defensively, unknown shape = "unavailable") |
| `omp session lease --json -- <ids>` → `{held, unknown}` | Crash recovery (HOST-28) | help, docs; rpc `get_session_lease` live |
| `omp usage reset` | Never run; the shell veto blocks it (9.2) | help |
| rpc `ready`, `negotiate_protocol`, `rpc_chunk` reassembly, 1 MiB frame cap | Transport | live (chunk rules docs) |
| rpc `get_state` (`model`, `thinkingLevel`, `sessionId`, `sessionFile`, `isStreaming`, `isCompacting`, `queuedMessageCount`, `isSettled`, `hasPendingAsyncWork`, `goal`, `usageLimit`) | Status, identity, readback | live (`sessionFile`, `usageLimit` docs) |
| rpc `prompt` with `streamingBehavior`, `follow_up`, `steer`, `abort` | Delivery, interrupt | docs |
| rpc `prompt_result` (`status`, `error.code`, `sessionSettled`), `session_settled` | Turn outcome, answer capture, quota errors | docs |
| rpc `set_model` (provider, modelId, thinkingLevel) | Model change with readback (HOST-18) | docs |
| rpc `goal` op `get/create/pause/resume/drop`, event `goal_updated`; setting `goal.continuationModes` with `rpc` | Lane goals (HOST-19) | live (`get`), docs (others) |
| rpc `set_ask_dialog` | Opt in to structured `ask` dialogs | live |
| rpc `set_host_tools` | Probe only (host-tool support) | live |
| rpc `get_session_lease` | Re-check before a resume | live |
| rpc `set_event_filter` | Drop `message_update` deltas the hub never reads | docs |
| Events `agent_start`, `agent_end` (`yielded`, `isTerminal`, `awaitingAsyncWork`), `turn_start`, `turn_end`, `message_end`, `tool_execution_start/end`, `queue_update`, `retry_fallback_applied`, `retry_fallback_succeeded`, `model_changed`, `notice`, `extension_error`, `rpc_frame_error` | Status, oversight, model watch | docs |
| `extension_ui_request` methods `select`, `confirm`, `input` (`secret` hint), `editor`, `ask`, `form`, `cancel`; `extension_ui_response` with `value`, `confirmed`, `cancelled`, `answers`, `data` | Dialogs, approvals, forms, spend guard | docs |
| End of stdin = drain, dispose, exit 0 | Close (HOST-20) | live |
| Settings `ask.timeout`, `goal.continuationModes`, `codexResets.autoRedeem5h`, `codexResets.autoRedeemWeekly`, `claudeResets.autoRedeem5h`, `claudeResets.autoRedeemWeekly`, `claudeResets.autoRedeemFable`, `retry.modelFallback` (default true), `retry.fallbackOn` (default `any`), `retry.fallbackChains`, `retry.fallbackRevertPolicy` (default `cooldown-expiry`: OMP restores the primary model itself), `tools.approvalMode`; `--config` overlays rank above project and global config | Overlay (4.2) | docs |
| Env `PI_NOTIFICATIONS=off`, `PI_CODING_AGENT_DIR` (default profile only), `PI_CONFIG_DIR` | Child env, isolated tests | docs |
| `models.yml` custom provider with `baseUrl` and `api: openai-completions` | Loopback fake model in tests | docs |
| Session file: header `type: session` with id, cwd, `parentSession`; entries `message` (roles `user`, `assistant`, `toolResult`; assistant `usage`), `model_usage`, `custom_message` with `details`, `compaction`, `session_exit` | Delivery confirmation, spend, answers, reopen checks | docs |
| Extension: `pi.on`, `registerTool` (with `defaultInactive`, late registration in `session_start`), `getActiveTools`/`setActiveTools`, `appendEntry`, `sendMessage` (`deliverAs` steer/followUp/nextTurn/aside, `triggerTurn`, `details`), `sendUserMessage`, `setModel`, `getThinkingLevel` | Bridge | docs |
| Extension context: `ctx.agent` (`kind`, `id`, `parentId`), `ctx.sessionManager.getSessionId/getSessionFile/getBranch`, `ctx.isIdle`, `ctx.hasPendingMessages`, `ctx.abort`, `ctx.shutdown`, `ctx.getAsyncJobSnapshot`, `ctx.models.current/resolve`, `ctx.ui.notify/setStatus`, `ctx.setInterval/setTimeout`, `ctx.sessionModes` (TUI only: goal enter/pause/resume, refused `busy` while a turn runs; installed 19.1.1) | Bridge | docs |
| Extension events: `session_start`, `session_before_switch`/`session_switch`, `session_clear`, `session_before_branch`/`session_branch`, `session_before_tree`/`session_tree`, `session_compact`, `session_shutdown`, `before_agent_start`, `before_provider_request`, `agent_start`/`agent_end`, `turn_start`/`turn_end`, `message_end`, `tool_call` (block result), `tool_execution_start/update/end`, `tool_approval_requested/resolved`, `plan_review_requested/resolved`, `goal_updated`, `session_stop` (continue with `additionalContext`), `before_subagent_spawn` (block), `retry_fallback_applied` | Bridge | docs |
| Global extension folder `<agent dir>/extensions/` (default `~/.omp/agent/extensions`) | Install (11.2) | docs |
| Extension `sessionManager` flush to disk | Taking ownership of a resumed transcript (EXT-35) | **unverified** (flush is documented for session storage, not for the extension's read-only manager) |
| Failing a provider request from `before_provider_request` | Model barrier (EXT-14) | **unverified**; design uses `ctx.abort()` from `before_agent_start` and `turn_start` instead |
| Model registry "subscription versus metered billing" | Model readback detail (EXT-14 requirement 40) | **unverified**; field omitted until found |
| Separate-realm worker's parent session ID in the extension | Shell rules for out-of-process subagents (EXT-04) | **unverified**; `ctx.agent.parentId` covers in-process subagents (docs) |
| Pinning one OAuth account per session (no automatic account rotation) | Only if the owner later counts rotation as "switching account" (12.2, row A14) | **unverified**; docs say stored accounts are "ranked and rotated automatically" |
| Exact `omp usage --json` field names | Quota job | **unverified** |
| Tern 0.9.33 CLI: `tern new tab [--cwd DIR] -- COMMAND`, `tern split`, `tern close`, `tern ls --json` | Tern runtime | help (Tern, third party) |

### 3.4 Probe, minimum version, newer versions

- **Minimum supported OMP: 19.1.1.** It is the release the contract suite first passes on. The binary embeds it
  and prints it in `cataly version --json` (IR requirement 5).
- **Probe** (HOST-03, HOST-35, PATCH-09, PATCH-13): at hub start, and when the configured `omp` path, size or
  mtime changes, run `omp --version` (5 s, 4 KiB cap). Then start one ephemeral rpc-ui child (flags as in 3.2),
  read `ready`, negotiate v2 when offered, and send `get_state`, `goal get`, `set_ask_dialog`,
  `get_session_lease`, `set_host_tools []`; end input; 20 s total. The result is a feature set
  (`goals`, `ask_dialog`, `lease_rpc`, `v2_chunks`, ...) cached in memory. Session starts never spawn a probe.
  A failed probe (an OMP that starts slowly just after an update, for one) is retried by itself after 5 s,
  15 s and 45 s, then every 5 minutes, until one works; `status`, health and the sessions refusal name the
  next try, and the lead starts as soon as a probe works.
- **Below minimum**: no session starts; health is red with found and required versions. Jobs that need no
  session keep running.
- **Unknown newer version**: allowed. Health shows "untested above X" until the nightly contract run passes on
  it. A capability the probe does not confirm reports "feature unavailable in this OMP" in the dependent
  command; nothing falls back silently. A different extension protocol version in the bridge hello gets role
  `none` plus one owner notice (IR requirement 19).
- **No upper pin.** A new OMP release works without touching Cataly unless OMP changes a public surface, and
  then the nightly run fails before the owner upgrades.

### 3.5 Decision: session control goes through rpc, the bridge carries only what rpc cannot

The extension spec (question 1) proposes control of headless sessions through the bridge connection; the
session-host, lane, intake and evidence specs assume rpc. **Decision: headless sessions are controlled only
over rpc-ui on the child's stdin/stdout.** The bridge connection of a headless session carries tool calls,
hook verdicts, evidence fingerprints and observation events, but never delivery, interrupt, model change,
goal or close.

Reasons: (1) **stable**: rpc is the documented host contract with request IDs, an ordered command queue,
`prompt_result` per prompt, `session_settled`, dialogs and goals; it exists from the first byte of the child
and dies with it, so there is no hello race and no second liveness signal to reconcile. (2) **fast**: a wake
is one pipe write, not socket → TypeScript → `sendMessage`. (3) **thin**: dialogs, approvals and forms reach a
host only through rpc anyway, so one channel sees everything that blocks a session. (4) It removes the
duplicated-control problem of the old design. The bridge stays the control path only for the tern runtime,
where no rpc channel exists. One `Runtime` trait (section 4) hides the difference from every caller.

Consequence for delivery: rpc has no "next turn without starting one" message. Quiet events and passive
notes therefore wait in the hub's inbox and go out with the next wake as one batch (COORD-10 rules 15-18),
instead of being sent as OMP next-turn messages. This also gives one batching rule for both runtimes.

## 4. Runtimes

### 4.1 One trait

`Runtime` is implemented twice: `Headless` (default) and `Tern` (optional, config only, HOST-30). Callers see:

```
start(spec) -> Started{session_id, file, model} | Failed(reason) | Unconfirmed
deliver(delivery) -> Delivered{turn_started} | NotReady(status) | Refused(reason) | InDoubt
interrupt() / set_model(sel) / goal(op) / answer(dialog_id, answer) / close(force)
snapshot() -> Snapshot      (never waits)
events() -> stream of SessionEvent
```

Both must provide: identity binding and readback (session ID, file, model, thinking); the status precedence
of HOST-12 (error > blocked > unknown > idle > working); turn start/end; dialogs as events; one change at a
time with immediate "busy" refusal; delivery receipts keyed by request ID (HOST-16); the output ring
(HOST-13); confirmation of every delivery from the session file (PATCH-01 rule 8).

### 4.2 Headless (default)

- Command line built from structured config (HOST-04, req. 17-18): one `omp` executable; the hub adds
  `--mode rpc-ui --config <overlay> --model <p/id> [--thinking L]` (no approval mode: decision 19) plus
  `--session-dir <dir>` (fresh) or `--resume=<file>` (resume), `--append-system-prompt <role file>` for the
  lead and lanes, `--tools <read-only set>` for look-only lanes (OPC-46), and `--max-cost <usd>` when a lane has
  a cost ceiling (an OMP-side backstop to WS-18). Extra owner flags may not touch any of these.
- The bridge is found through OMP's global extension folder (11.2), not passed with `-e`. Passing `-e` as
  well would load it twice. Health fails, and starts are refused, if the extension link is missing or stale.
- Overlay file (written once per hub run, mode 0600): `ask.timeout: 0`; `goal.continuationModes` including
  `rpc`; all five saved-reset keys `no`; `retry.modelFallback: true` and `retry.fallbackRevertPolicy:
  cooldown-expiry` (both OMP's defaults, pinned so an owner config cannot silently differ); `retry.fallbackOn`
  and `retry.fallbackChains` only when set in Cataly's `[fallback]` config (9.2).
- Child environment starts empty: `PATH`, `HOME`, `TMPDIR`, `LANG`, listed provider credentials,
  `PI_NOTIFICATIONS=off`, `CATALY_LAUNCH=<single-use token>`, `CATALY_SOCKET=<path>`. No relay token, no
  Cataly secret, nothing secret in argv (HOST-06, req. 21).
- Start sequence (HOST req. 23-28): record first, spawn, `ready` within 15 s, two equal `get_state`
  answers bind identity, model readback, bridge hello within the start budget (a lane without its bridge
  cannot have shell rules, so it fails closed), 30 s total (lanes 180 s, LANE-15).
- Close: end stdin, wait 15 s, SIGTERM, 5 s, SIGKILL; signals only to the hub's own unreaped child.
- At binding time the transcript is whatever `get_state` reports, not the path the hub asked for: OMP gives a
  process a fresh sibling file (new session ID, `parentSession` set) when another live process already holds the
  transcript's lease, and the hub binds the reported pair and reports the move once. After binding, any different
  pair faults the session (HOST req. 26); the two rules do not overlap (docs, session storage).
- Goals use rpc `goal` (verified), so lanes keep OMP goal mode (HOST-19; LANE req. 15g, 44-45).

### 4.3 Tern (optional)

- Start: `tern new tab [session] --cwd <dir> -- /usr/bin/env CATALY_LAUNCH=<token> CATALY_SOCKET=<path> omp
  <flags>` with the same flags as headless except mode and approval mode (HOST req. 93). The token is single-use and
  expires after 60 s, so showing it in the process list grants nothing lasting.
- Identity, status, goal state and events come from the bridge hello and event stream (HOST req. 94-97).
- Control through the bridge: deliver (`sendMessage` custom message with the delivery ID in `details`; wake =
  `triggerTurn`, steer = `sendUserMessage` steer), interrupt (`ctx.abort`), model (`pi.setModel`), goal
  create/pause/resume (`ctx.sessionModes.set`, TUI only; OMP takes a mode change only between turns, so a tern
  lane's goal is created before its kickoff), shutdown (`ctx.shutdown`).
- Dialogs belong to the person at the terminal. The hub observes approvals and reports the session blocked; it
  never answers. Forms may be mirrored to the owner's thread (EXT requirement 42); first answer wins.
- Session switches by the user are detected and fenced as in PATCH-01 rules 6-9 and 12-14.
- Tern lanes support the full lifecycle (open, pause, resume, park, abandon, claim, close, reopen); only remote
  dialog answers are missing. Losing the bridge connection (45 s without heartbeat) marks the lane lost.

## 5. Relays and voice

### 5.1 One relay trait

Each relay (Discord, Telegram, Slack) implements one trait; everything after normalisation is shared code.

| Capability | Contract |
| --- | --- |
| Identity | Start-up check of the account (DISC-IN-01/02, TELE-01, SLACK-03); state bound to (account, owner) and refused if either changes. |
| Intake | Live stream (Gateway, long poll, Socket Mode) plus catch-up reads that start from a durable position. Emits one normalised `Inbound` (MSG-01) with a stable dedupe key. |
| Owner gate | Per relay: allowlisted sender IDs, not a bot, not the account itself, right room/topic (DISC-IN-12, TELE-04, SLACK-08). Anything else is dropped before receipt or hand-off. Text that arrives inside a request but was not written by the owner (quotes, reply parents, forwards) is labelled data (OPC-08/10/11). |
| Commit | Position, queue removal and hand-off receipt in one store transaction (DISC-IN req. 49, MSG-03). |
| Send / edit / upload | One `SendRequest` shape; outcome Confirmed(ids) / Rejected / Uncertain (MSG-05, DISC-OUT-06). Uncertain is never repeated unless the write carries an enforced deterministic nonce (Discord) or is naturally idempotent. |
| Split limit | Discord 2000 UTF-16 units with fence repair (DISC-OUT-03), Telegram 4096 (TELE-08), Slack per API. |
| Seen mark | 👀 on admission, in parallel with hand-off; failure logged, never re-admits (MSG-04, DISC-OUT-08). |
| Typing | Discord only, driven by session state (OJ-30, owner-jobs requirement 57). Other relays report "unsupported". |
| Questions | Post a form or approval card, receive answers as `Inbound::Answer` (buttons on Discord; replies on all). |
| Threads | Create, rename, archive, status prefix (Discord threads and forum posts; Slack thread memory). |
| Health | Connected, last good event, receive-lag percentiles (TELE-07, MSG requirement 15). |

Shared pipeline, one implementation: normalise → gate → durable arrival queue → route (open question answer,
then the room's owning session, then the lead; DISC-IN-13) → hand-off through the target's inbox → confirm
in the session file → commit. One HTTP client and policy for all relays (MSG-06, DISC-OUT-23): 5 s connect,
10 s per ordinary call (15 s for Telegram and Slack per MSG requirement 9), no redirects, 2 MiB body cap,
reads retried with backoff, writes never retried after an uncertain outcome. Tokens never appear in logs.

**Hand-off confirmation.** A headless delivery's text starts with a short Cataly envelope line carrying the
delivery ID and, for requests, the room and message IDs. The delivery is confirmed when that line appears in a
`user` entry of the target session file (read incrementally from the last offset after the `message_end`
event). Tern deliveries are confirmed by the delivery ID in a `custom_message` entry's `details`.

**Answers.** The hub posts a session's final answer itself (DISC-OUT-07): the last assistant message of the
turn the request started, read from the session file after `prompt_result`, table-rendered, split, stored in
the outbox part by part, and posted as a linked reply. The stop gate (EXT-24) remains as the rare fallback.

### 5.2 Voice is a separate module

Voice (VOICE-01 to VOICE-24) is the `cataly-voice` crate behind a Cargo feature, so a build without it carries
no audio code (10.1). It reaches the hub only through its `VoiceHost` trait: Discord voice events from the one
Gateway connection (VOICE-03), the lead-delivery API with receipt proof (VOICE-15), status reads and note posts.
Its parts: Discord voice connection with DAVE end-to-end encryption, Opus decode/encode, 24 kHz resampling and
pacing, a realtime model session with delegated tools, a usage ledger with hard caps, and the playback-proof
queue for lead replies. It has its own spending rules (9.2).

**Two transports to the realtime model, one session logic (owner decision 11, VOICE-11, VOICE-12).** The
session state machine, tools, ledger, pacing and playback proof are written once against a `RealtimeLink`
trait with two implementations: WebSocket (PCM frames in JSON events) and WebRTC (SDP offer posted over HTTPS,
then Opus over SRTP plus a data channel for events, through str0m). Config selects the transport; the other
is not started. Provider standby stays dropped: idle close plus wake on speech covers it. Both transports are
tested in CI against a fake realtime server and, before each release that changes voice, against the real
API under the gated live test of `docs/plan.md` milestone 5. OMP's own live voice (`live_connect`, docs) is not
used: it binds voice to the lead's process and Codex sign-in and has no spending ledger.

## 6. Job engine

### 6.1 One table, one stream

Every timed or watching behaviour is a row in one `jobs` table plus a built-in **job kind** in Rust:

| Kind | Source spec | Cadence (default / minimum) | Change rule |
| --- | --- | --- | --- |
| `cron`, `once`, `probe`, `stream` | WS-26 to WS-30, WS-02 | trigger / 30 s for probes | per WS requirements 52-75 |
| `pr` (follower) | WS-24, WS-25 | 3 min | new item, state difference |
| `github` (owner PRs + notifications) | OJ-05, OJ-06 | 3 min / 1 min | new item |
| `quota` (+ limits view, quota gate) | OJ-07 to OJ-09, WS-23 | 5 min / 2 min | level crossing |
| `model` (fallback watch) | OJ-10 to OJ-12 | on events + 60 s | state difference |
| `spend` (spend-offer guard) | OJ-13 | on every dialog + 60 s sweep | action, reported every time; cannot be disabled or dry |
| `backup` (git + secret scan) | OJ-15 | 30 min / 5 min | health (hit set), action |
| `health` (self health, file age) | OJ-16, OJ-18, WS-34 | 2 min | health, 2 strikes |
| `items` (new-items watch, e.g. mail) | OJ-19 | 30 min / 5 min | new item |
| `digest` (owner return) | OJ-20, OJ-21 | on owner message | — |
| `unanswered` | OJ-22 | 5 min | new item by age |
| `oversight` (per lane) | WS-10 to WS-22, COORD-06 | deadline-driven + 5 min sweep | state difference |
| `pressure` (host) | LANE-09 | 60 s | state difference |
| `planner` | COORD-19 to COORD-36 | 1 h | — |
| `board` (board posts) | EVR-30, OPC-25 | on change, ≤ 1 per 60 s per destination | body difference |
| `update` (release notice) | IR-21, IR-22 | 24 h | new version |

**Model watch with fallback on (owner decision 7).** A session on a different model than requested is now
expected after a quota wall, so the `model` job reports, it does not repair: one quiet event per distinct
fallback (`retry_fallback_applied`: from, to, reason), one note when the requested model serves again, and an
urgent event only for "no model" or for a model OMP did not report as a fallback. Returning to the requested
model is OMP's own `cooldown-expiry` revert, pinned in the overlay (OJ-11); the hub has no second return path,
and the old 10/20/40-minute recovery ladder is not built. If a session is still off model an hour after the
requested provider's reset time from the quota job, the owner is told once. The lane model pin follows the
same rule: the bridge's barrier (EXT-14) accepts the requested model and any model OMP itself announced as the
fallback target in that session, and aborts on anything else. The start-time readback (HOST req. 27) still
requires the requested model exactly.

A job instance is configuration: name, kind, enabled, cadence or trigger, per-tick deadline, dry-run flag,
destinations, kind settings (OJ-01). Several instances of one kind may exist (two backups, two mail watches).

Every job reports through one typed `Event`: time, job, subject, kind, severity, text (≤ 400 chars), optional
body, lane and thread links, urgent flag, dry flag (WS-01 requirement 2). Events are appended to the store and
to a rotating owner-readable log (10 MiB), then routed: to the lead's inbox (urgent wakes, quiet waits), to
relay destinations, or kept locally. Messenger posts never mention anyone and pass the outgoing text checks
(OJ-26).

### 6.2 Common rules, written once

- Transition memory per (job, subject, condition), persisted; emit only on change; one warning per distinct
  failure reason; one recovery note; actions taken are reported every time (OJ-01, WS-01 requirement 4).
- First tick takes a baseline and reports nothing that already existed, except health and spend problems.
- State is committed after the tick's events are in the stream: a crash may repeat an event, never lose one.
- Per-tick deadline; an overrun cancels the tick, is reported once, and the next tick runs on schedule.
- Commands: argument vector only, no shell; one per job, four across the hub; timeout, output cap, TERM then
  KILL after 5 s; stdin closed; output to private files (WS req. 59-60, OJ req. 9).
- Dry run: full tick, events marked dry, no post, no action; dry state kept apart and discarded on going live.
  "Run once" (dry) is open to the owner and the lead.
- Authority: only the owner changes jobs that are owner jobs; the lead may list, run once dry, and manage its own
  stream jobs and timed jobs (WS req. 51, 74); lanes manage nothing.
- The scheduler sleeps until the next due time (recomputed on change and on a wall-clock jump over 30 s); no
  polling loop.

## 7. Storage

### 7.1 What goes where

| Data | Store | Why |
| --- | --- | --- |
| All records: sessions, receipts, deliveries, lanes and history, reports, steps, evidence, questions, grants, request log, owed replies, outbox, relay positions, arrival queue, inboxes, jobs, runs, transition memory, events, PR cursors, backlog, slots history, verdicts, voice ledger and reply queue, update state | **SQLite**, one file `<state>/cataly.db` | Many small records that must change together in one transaction (hand-off = queue removal + position + receipt, DISC-IN req. 49). WAL gives crash safety without the lock files, temp-file renames and torn-line repair the old product needed (EVR-41). One file to back up. Bundled SQLite means no system dependency. |
| Configuration | TOML file `<config>/config.toml` | Owner-edited; strict validation; reload on request (WS-35). |
| Secrets (relay tokens, voice key) | One file each in `<config>/secrets/`, 0600 | Read only by the hub; never in config, env of children, or logs (OPC-87). |
| Job run output, spill bodies over 64 KiB, stderr logs of children (1 MiB cap), digest bodies | Files under `<state>/files/`, 0600 | Large opaque blobs; the store keeps paths and hashes. |
| Event log for the owner | Rotating JSON-lines file, 10 MiB | Readable with ordinary tools (WS requirement 7). |
| Profile, published-file registry roots | Owner's files | The lead edits the profile; the hub only checks, snapshots and reverts it. |
| Transcripts | OMP's session files | Owned by OMP; the hub only reads them. |

### 7.2 Rules

- **Single writer.** One dedicated thread owns the only read-write connection. Every mutation is a typed
  command sent to it; it batches commands that arrive together into one transaction. Reads use a small pool
  of read-only connections (WAL readers never block the writer).
- **Durability.** `journal_mode=WAL`, `synchronous=FULL`, `foreign_keys=ON`, `busy_timeout=0` (no other writer
  exists). FULL costs one fsync per commit. The owner's wake does not wait for it: decision 15 writes an owner
  request to the lead while its arrival, delivery and hand-off records commit, and recovery reads the lead's
  transcript before any resend (9.3). Weaker durability stays ruled out.
- **Owner-only files.** State and config directories 0700, files 0600; symlinks, foreign owners and loose modes
  are refused (HOST-37).
- **Bounds.** Every growing table has a retention rule from its spec (7 days of delivered hand-offs, 512 receipts
  per session, 1,000 backlog events or 7 days, 168 planner runs, 30 days of once-only identities, ...) applied by
  a daily maintenance job; page cache capped at 2 MiB.
- **Migrations.** Ordered SQL files embedded in the binary; `PRAGMA user_version` is the schema version. Before
  a migration the hub writes `VACUUM INTO <state>/backup/cataly-v<N>.db`. Each migration declares whether it is
  additive; the store records the lowest binary version that can still open it. A rollback binary older than
  that refuses to start and names the backup to restore. No down-migrations.

### 7.3 Schema outline

| Group | Tables |
| --- | --- |
| Meta | `meta` (schema version, min reader, install ID), `bindings` (relay account + owner pairs) |
| Sessions | `sessions` (handle, role, runtime, cwd, model, state, bound IDs, stop record), `receipts`, `deliveries` (PATCH-01 states), `inbox` (target, sender, text, state, identity), `once_ids` |
| Lanes | `lanes`, `lane_history`, `lane_runs`, `queued_launches`, `open_dedupe`, `shell_rules`, `shell_grants`, `worktrees` |
| Evidence | `reports`, `steps`, `step_receipts`, `evidence_seen`, `published_files`, `file_versions`, `learned_rules` |
| Owner | `questions`, `grants`, `requests` (request log), `owed_replies`, `consent`, `thread_owner` |
| Relays | `positions`, `arrival_queue`, `handoffs`, `outbox_parts`, `read_marks`, `thread_journal`, `room_notes`, `recall_rows`, `slack_threads`, `status_posts` |
| Jobs | `jobs`, `job_runs`, `job_memory` (transition memory), `events`, `event_cursors`, `prs`, `pr_cursors`, `backlog_items`, `planner_runs`, `verdicts` |
| Voice | `voice_days`, `voice_replies`, `voice_state` |
| Release | `update_state` (last check, announced, silenced) |

## 8. Control plane

- **One socket.** Unix stream socket `hub.sock` in a 0700 directory the hub creates: `$XDG_RUNTIME_DIR/cataly/`
  on Linux, `/tmp/cataly-<uid>/` on macOS (its per-user temp path is too long for the 103-byte limit). The
  socket is 0600. A stale socket is removed only after a refused connect; the exclusive `hub.lock` is the real
  single-instance guard (DISC-IN-25, WS-32).
- **Authentication by the file system.** The directory and socket modes, plus a peer-credential check
  (Tokio's `peer_cred`: `SO_PEERCRED` on Linux, `getpeereid` on macOS) that the connecting UID equals the
  hub's. Other UIDs are closed without a reply (HOST req. 72).
- **Authority above that.** Same-UID processes are not a security boundary, so authority is assigned, not
  claimed: a bridge connection gets the role the hub gives its session at hello (lead, lane with key and run,
  none); a CLI connection is the owner, unless its peer PID descends from an OMP child the hub spawned, in which
  case it gets that session's role. In addition, the bridge's shell veto blocks lanes from running the `cataly`
  binary except read-only verbs (section 9.1). This guards against model mistakes; it does not claim to stop
  deliberate escape by code running as the owner.
- **Protocol.** Newline-delimited JSON, at most 1 MiB per line, 64 connections, 60 s idle close (HOST req. 73-74).
  Request `{v, id, method, params}`; reply `{v, id, ok}` or `{v, id, err: {code, message}}`. Streams
  (`events.follow`, `session.follow`) send `{v, id, item}` lines until cancelled. A request runs to completion
  even if its caller disconnects; every mutating method takes an optional caller request ID for dedupe.
- **Bridge link.** Same socket: `hello` (protocol version, OMP version, launch token, session ID and file, PID,
  cwd, agent kind, run mode) → role answer with guidance text, tools to show, model pin, shell-rules flag. Then
  events with per-connection sequence numbers, heartbeats every 15 s (45 s = closed), bounded buffering on the
  bridge side (1,000 events or 1 MiB), snapshot on reconnect (EXT requirements 3-16).
- **Versioning.** `v` is the control protocol major (starts at 1). Fields are only added within a major;
  unknown fields are ignored; unknown methods answer `unknown_method`. The bridge carries its own extension
  protocol version; a mismatch gets role `none` and an owner notice. The CLI and hub come from one binary, so
  they only differ for the seconds of an update; a mismatch answers "restart the hub".

## 9. Safety rules enforced in code

### 9.1 Owner-only authority

- Every control method declares the authority it needs: `Owner`, `Lead`, `Lane(own key)`, `Job`, `Hub`.
  One dispatcher checks it; handlers never re-derive authority from text (OPC-10).
- A request is admitted only through a relay's owner gate. Text inside it that the owner did not write is
  passed in a separate data field (OPC-08, OPC-11).
- Authorisations come only from an owner message or an owner button press bound by reply reference (OPC-05,
  OPC-09, OPC-51). Timeouts, silence, memory and recommendations never authorise.
- Lane tools cannot open, close, merge, post to the owner or grant themselves exceptions (OPC-57, LANE req. 74).
- The lead's goal tool may not create goals (the lead idles without one, OPC-36); lane goals are set by the hub
  from the brief and done checks. OMP itself refuses a second goal (docs).

### 9.2 Never spend (fallback allowed)

Owner decision 7: a session may fall back to another configured model when a quota runs out. Redeeming saved
resets, buying credits and paid overage stay refused, in code:

- Overlay sets every saved-reset key to `no`; the hub never runs `omp usage reset` and the bridge's shell veto
  blocks it in every session as a non-overridable rule (OJ-13, OJ requirement 38).
- Spend-offer dialogs (built-in phrases plus owner phrases) are answered with cancel within 1 s, never with
  "no" or a remembered choice; a dialog that stays is cancelled again, the session is fenced, and the owner is
  alerted. Three offers in 10 minutes fence the session.
- `get_state.usageLimit` reporting `extraUsage` fences the session before its next request, because that
  request could bill overage.
- **Fallback is OMP's, not Cataly's.** The overlay sets `retry.modelFallback: true`. Which errors walk a chain
  (`retry.fallbackOn`) and the chains themselves (`retry.fallbackChains`) come from the owner's OMP config,
  or from Cataly's `[fallback]` config section when the owner sets it there, in which case the overlay carries
  it. Cataly never invents a chain. OMP only falls back along a configured chain, so with none configured,
  "on" changes nothing and health shows that as INFO.
- The hub itself changes a session's model only back to that session's requested model, with readback; it has
  no code path that picks another provider, account or model.
- A fallback to a metered (API-key) model bills per token. That is ordinary model spend, not credits or
  overage, and it is bounded by the lane's cost ceiling, which also goes to OMP as `--max-cost` (WS-18).
- Quota exhausted for good (a turn ends with a usage-limit error in `prompt_result`, meaning the chain is
  exhausted or absent) marks the session quota-blocked: no nudges, reminders or deliveries other than owner
  requests; the lead is told once (OPC-58, OPC-64).
- Guidance text for the lead and lanes says: never redeem a reset, buy credits, enable overage or switch
  account, provider or model yourself; OMP's configured fallback may change the model for you; when it is
  exhausted, stop, save progress and report. This replaces the stricter "no switching" wording in LANE req. 13
  and OPC-63/64.
- The quota gate keeps new session starts (a fresh lead, lanes, planner starts) waiting at the hold level;
  resuming an existing session (the lead's last one, a parked or live lane) never waits; running sessions are
  untouched; only the owner can lift it. A held lead is said with its provider, limits, reset times and the lift.
- Voice is the one paid feature: explicit opt-in, owner-entered prices, daily, total and spending caps checked
  before every call starts, hold after any failure, never a paid retry (VOICE-19, VOICE-20).

### 9.3 Fail closed on uncertain delivery

- Every outside write ends confirmed, rejected or in doubt. In doubt is never repeated automatically, with
  three exceptions written in code: a Discord create carrying an enforced deterministic nonce; an owner
  request whose target session file is final (process gone) and does not contain it (DISC-IN req. 50); a tern
  delivery resent once with the same delivery ID, which the bridge deduplicates (PATCH-01 rule 11).
- Hub notices in doubt are never resent; the lead is told once and may confirm or release (COORD-13).
  Lead-to-lane messages in doubt fence the lane's inbox until the lead releases it (EVR req. 62).
- A message found in another session's file is "misdelivered": never resent, owner told once.
- **Early wake (decision 15).** An owner request from a relay that goes to the lead alone is written before its
  records commit, under a delivery marker derived from its request ID; on Discord, whose Gateway never
  replays a message to a new session, a live message is also handed off while its arrival commits (the
  room's position stays below it until then, so a crash leaves it to the catch-up read). A message not read
  live in this hub run (restart, catch-up, a hand-off made again) is looked up in the current and previous
  lead transcripts before any send: found means delivered; a transcript that cannot be searched holds the
  room; with no lead running yet it waits. Written with a record that then fails to commit is in doubt, never
  resent. Senders that cannot prove a request was not written before (Telegram and Slack redeliver live,
  `cataly lead send` retries with the same request ID, lane messages, notes) commit first.
- Remote approvals (off by default) deny on any doubt: a card that cannot be posted, an unacknowledged press,
  or a run that ended.

### 9.4 Bounds on every external call

Every call to OMP, the network, the file system or a subprocess has a timeout, a size cap and a recovery
path. The values come from the specs; the main ones:

| Call | Bound |
| --- | --- |
| `omp --version` / probe / start / rpc response / stdin write | 5 s, 4 KiB / 20 s / 30 s (lanes 180 s) / 15 s / 15 s |
| OMP close | 15 s, then TERM, 5 s, KILL |
| `omp session lease`, `omp usage --json` | 10 s / 15 s, 1 MiB |
| rpc frame | 1 MiB per line; 64 MiB reassembled on v2 |
| Discord REST / Gateway | 5 s connect, 10 s total, 2 MiB / connect 10 s, HELLO 10 s, READY 20 s, frame write 3 s, frame 1 MiB |
| Telegram long poll / Slack calls | 25 s wait + 10 s / 15 s |
| `gh` / git local / git push | 60 s (GitHub reads in done checks 120 s) / 60 s / 300 s |
| Job commands / event commands | per job (default 30 s, max 1 day) / 30 s, 4 at a time |
| Bridge: connect / report tool / job tool / shell veto / stop gate | 1 s / 10 s / 5 s / 2 s / 1 s |
| Session file reads | header 64 KiB (1 MiB for reopen), in-doubt search last 8 MiB and 2 s, file ≤ 64 MiB |
| Release check / install downloads | 15 s, 1 MiB / connect 10 s, 120 s per asset, binary ≤ 128 MiB |

### 9.5 No panics

- `unsafe` is forbidden in every crate (`#![forbid(unsafe_code)]` through workspace lints).
- Clippy denies `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `indexing_slicing`,
  `dbg_macro` and `print_stdout` outside the CLI output module; tests are exempt.
- Errors are typed per module (plain enums with hand-written `Display`) and carry one class used for decisions: `Transient` (retry with
  backoff), `Rejected` (tell the caller), `InDoubt` (fence, never retry), `Fatal` (stop the task).
- Release builds keep `panic = "unwind"` so a bug in one task is caught at its supervisor (2.2) instead of
  ending every session; a panic is logged with its location and counted in health.

## 10. Workspace, dependencies, lints, tests, CI

### 10.1 Layout: two crates

Owner decision 13 asks for the lightest structure with one implementation per concern. A crate boundary is
kept only where it buys something concrete; everything else is a module of one crate, separated by module
privacy (`pub(crate)` at most) rather than by packages.

| Crate | Contents | Why it is its own crate |
| --- | --- | --- |
| `cataly` (one package, `lib` + `bin` targets) | Modules: `cli`, `hub` (start-up, supervision, socket, dispatcher), `proto` (control and bridge messages), `sys` (paths, modes, atomic writes, locks, peer credentials, host readers, signals, sd_notify, stall detector), `log`, `store`, `omp` (rpc-ui client, launch, overlay, probe, session-file reader, usage and lease parsers), `host` (`Runtime` trait, headless, tern, session actors, deliveries, inboxes), `relay` (trait, pipeline, `http`, `text`, `discord`, `telegram`, `slack`), `jobs` (scheduler, events, routing, job kinds), `lanes` (lifecycle, reports, evidence, done checks, oversight, planner, slots, questions, grants, review gate, profile) | The product. The `lib` target exists only so integration tests and the contract suite can call modules directly. |
| `cataly-voice` (optional dependency behind the `voice` feature) | Discord voice connection, DAVE, Opus, resampling and pacing, realtime model over WebSocket and WebRTC, usage ledger, reply playback queue | (1) **Dependency kept out:** libopus (C), the DAVE/MLS stack and the WebRTC stack compile and link only in voice builds. (2) **Feature gate:** a build without voice contains none of it. (3) **Test seam:** the crate defines a small `VoiceHost` trait (lead delivery with receipt, status read, note post, Gateway voice events in); `cataly` implements it, and voice tests run against a fake host and a fake realtime server. Cargo forbids a dependency cycle, so the narrow trait is enforced by construction. |
| `extension/cataly.ts` | The bridge, under 600 lines, with its own tests | Not Rust; OMP loads TypeScript. |

Removed boundaries, and why nothing is lost: `proto`, `sys`, `store`, `omp`, `host`, `relay`, `jobs` and `lanes`
had no feature gate, no dependency that only they use, and no consumer outside the hub, so as crates they
bought only compile-time parallelism. `testkit` becomes `tests/support/` (fakes use dev-dependencies, which
never enter the product). If the incremental debug build of `cataly` exceeds 30 s on the CI runner, the module
with the largest share is split out then, along a module line that already exists; not before.

**One implementation per concern.** Shared modules and their users, so nothing is written twice:

| Concern | One module | Used by |
| --- | --- | --- |
| rpc-ui framing, correlation, v2 chunks | `omp::rpc` | probe, headless runtime, health check, contract suite |
| Session-file reading (incremental, bounded, file identity checks) | `omp::transcript` | delivery confirmation, answer capture, in-doubt search, spend totals, model watch, reopen header checks |
| HTTP policy (deadlines, caps, retries, 429, outcome classes) | `relay::http` | all relays, voice signalling, GitHub release check, web done checks |
| Text splitting, table rendering, width and cuts | `relay::text` | all relays, board, CLI output |
| Delivery into a session (inbox, batching, in doubt, receipts) | `host::inbox` | relay hand-off, job events, oversight, lead-to-lane messages, voice pass-to-lead |
| Transition memory and once-per-change emission | `jobs::memory` | every job kind, pressure notices, oversight |
| Bounded subprocess (argv, timeout, cap, TERM then KILL) | `sys::command` | `omp --version`, usage, lease, `gh`, `git`, job and event commands, done checks |
| Atomic owner-only file write | `sys::fs` | overlay, spill files, config setup, install |
| Secret-shape masking | `sys::secrets` | backup scan, outbound posts, recall store, profile check |

No `build.rs` beyond embedding the version and migrations. The extension's version constant is generated from
the Cargo manifest at release time (IR requirement 6).

### 10.2 Dependencies

Policy: few, well-known crates; each recorded in `docs/third-party.md` with licence and source (cleanroom rule
4); `cargo deny` allows only MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, Unicode-3.0, CDLA-Permissive-2.0 and
public domain (SQLite); no copyleft in the binary. Default features off; each enabled feature is justified.
A crate stays only if writing its job in-house would be large or risky; each row says what it saves. Licences
checked on crates.io on 2026-11-25.

| Crate (core) | Licence | What it saves |
| --- | --- | --- |
| tokio (rt-multi-thread, net, process, time, signal, sync, io-util, fs) | MIT | An async runtime, child-process pipes, Unix sockets, signal handling and peer credentials (`UnixStream::peer_cred`, `getpeereid` on macOS) |
| serde, serde_json | MIT OR Apache-2.0 | Correct JSON for every wire format and the store's JSON columns |
| rusqlite (bundled) | MIT; SQLite public domain | A crash-safe transactional store (section 7) |
| reqwest (`rustls-no-provider`, `json`, no defaults) | MIT OR Apache-2.0 | Connection pooling, TLS wiring, redirect and timeout policy. Kept on measurement terms: if replacing it with `hyper-util` directly saves over 1 MiB of binary or resident memory in milestone 1, `relay::http` is rewritten on hyper. |
| ring (as the rustls crypto provider, also used directly) | Apache-2.0 AND ISC | Already in the tree for TLS, so it also supplies SHA-256, secure random tokens and nonces, ed25519 release-signature verification and AES-GCM for Discord voice transport. Replaces sha2, getrandom and ed25519-dalek. |
| tokio-tungstenite (rustls) | MIT | WebSocket framing for the Discord Gateway, Slack Socket Mode and the realtime model |
| futures-util (`sink`, no defaults) | MIT OR Apache-2.0 | The stream and sink traits of the WebSocket stream; adds no weight because tokio-tungstenite already depends on it |
| toml | MIT OR Apache-2.0 | A full TOML parser for the owner-edited config |
| jiff | Unlicense OR MIT | IANA time zones, DST gaps and repeats (WS req. 52), ISO instants |
| regex | MIT OR Apache-2.0 | A linear-time engine for owner-supplied patterns: shell rules, stream filters, secret patterns, spend phrases |
| unicode-width, unicode-segmentation | MIT OR Apache-2.0 | Unicode tables for display width and grapheme-safe cuts (DISC-OUT req. 43, COORD req. 64) |
| rustix (fs, process) | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | Safe `statvfs`, `getrusage` and `kill(SIGTERM)` to own children without `unsafe` |
| base64 | MIT OR Apache-2.0 | Strict decoding for rpc v2 chunks and avatar data URIs; adds no weight because reqwest already depends on it. If reqwest goes, a 60-line in-house decoder replaces it. |

| Crate (`cataly-voice` only) | Licence | What it saves |
| --- | --- | --- |
| davey | MIT | Discord's DAVE end-to-end encryption (MLS); brings openmls and RustCrypto, voice builds only |
| audiopus (bundled libopus) | ISC; libopus BSD-3-Clause | The Opus codec |
| str0m (`rust-crypto` backend, no default features) | MIT OR Apache-2.0 | Sans-I/O WebRTC (ICE, DTLS, SRTP) for the realtime model's WebRTC transport, with no C crypto |

Milestone 5 note (`docs/notes/voice.md`): the voice build pulls in three things this table does not name.
(1) chacha20poly1305 (Apache-2.0 OR MIT) for Discord's XChaCha20 transport mode, the fallback when a voice
server offers no AES-256-GCM. (2) str0m's DTLS comes from dimpl, which links AWS-LC (aws-lc-rs/aws-lc-sys,
ISC and Apache-2.0 family), so "no C crypto" does not hold for the WebRTC transport; every licence is on the
allow list. (3) davey's MLS stack uses the hpke-rs crates, licensed MPL-2.0 (file-level copyleft), used
unmodified with their source published; `deny.toml` allows MPL-2.0 for the hpke-rs crates alone. All three
compile only into voice builds.

| Crate (dev only) | Licence | What it saves |
| --- | --- | --- |
| proptest | MIT OR Apache-2.0 | Property tests with shrinking for the splitter, tables, cron and owner gate |
| hyper (server feature) | MIT | Fake relay and model servers; already in the tree as reqwest's client |

Removed from the first draft and written in-house instead: **lexopt** (the CLI grammar is "verb, then flags
whose value is the next word verbatim", COORD-53, about 150 lines), **tracing / tracing-subscriber** (a `log`
module writing JSON lines with level and key-values to stderr or a file; the hub has no span tree to justify
them), **thiserror** (hand-written `Display` for a few error enums), **sha2 / getrandom / ed25519-dalek**
(supplied by ring), **sysctl** (macOS memory level read with a bounded `sysctl -n kern.memorystatus_level`
subprocess), **tempfile** (a test helper making unique directories under the target folder), **songbird**
(too large; the Discord voice protocol pieces are written in
`cataly-voice` on tokio, ring, davey and audiopus). The hub lock uses `std::fs::File::try_lock` (stable since
Rust 1.89.0). Cron parsing, the HTML form reader (DISC-OUT req. 60) and sd_notify (`std` Unix datagram) are
in-house because the specs define them exactly.

### 10.3 Lints and style

Workspace `[lints]`: `unsafe_code = "forbid"`, `missing_docs = "warn"` on public items, Clippy `pedantic` as
warn plus the deny list in 9.5; CI runs with `-D warnings`. `rustfmt` with defaults. Pinned toolchain in
`rust-toolchain.toml`. Bridge: TypeScript strict mode, type-checked against the public extension types of each
OMP version under test, `biome` for lint and format.

### 10.4 Tests

| Level | What | Where |
| --- | --- | --- |
| Unit | Pure rules: status precedence, splitting, table rendering, cron, owner gate, transition memory, done checks, shell rules | next to each module; property tests where the spec states an invariant ("applying twice changes nothing", "parts joined equal the input") |
| Store | Migrations forward from every released schema; crash injection between write and commit | `store` module |
| Fake servers | Discord Gateway + REST (resume, op 7/9, close codes, 429 buckets, nonces), Telegram long poll (409, 429, offsets), Slack Socket Mode and Web API, OpenAI-compatible model with scripted replies, realtime model (WebSocket and WebRTC signalling), GitHub via a fake `gh` | `tests/support/`, loopback only (DISC-OUT-27) |
| Contract suite | Stock OMP in an isolated agent directory (`PI_CODING_AGENT_DIR`) with the loopback model in `models.yml`: probe; ready and negotiation; prompt, follow-up, steer, abort; `prompt_result` and `session_settled`; dialogs of each method; goals; resume and identity checks; lease; bridge load, hello, inert behaviour in ordinary sessions; delivery confirmation from the session file; PATCH-01 rules through real session changes | `tests/contract`, 30 s per case, 10 min total (PATCH-12) |
| End to end | Hub + fake relays + stock OMP: request to answer posted; lane open to close; job delivery; restart recovery | `tests/e2e` |
| Benchmarks | Section 2.3 measures | `tests/bench`, results kept as CI artefacts |
| Live voice | Both transports against the real realtime API: connect, one spoken exchange from recorded audio, pass-to-lead with receipt, cap stop | `tests/live_voice`, ignored by default; runs only with the owner's key in the environment and an explicit spending limit the owner approved; never in CI |

### 10.5 CI matrix

| Workflow | Runners | Contents |
| --- | --- | --- |
| On every pull request and push to `main` | `ubuntu-latest` (x64), `macos-latest` (arm64) | fmt, clippy `-D warnings`, tests with `--locked`, `cargo deny`, bridge type check and tests, release build, contract suite against OMP 19.1.1 (the minimum) and the newest OMP known when the change was made, each downloaded from OMP's official release assets and checked against its published `SHA256SUMS.txt`, memory and latency benchmarks compared with `main` |
| Nightly | same two | Contract suite and e2e against the latest OMP release resolved at run time; failure notifies the owner through GitHub (IR-38) |
| Release (tag `v*`) | same two, then publish | Section 11.4 |

Third-party actions are pinned by commit hash; workflow permissions are read-only except the publish job.

### 10.6 The bridge in one page

Kept, in order of necessity (EXT "what must remain"): recognise a Cataly session (launch token or session
marker entry) and stay inert otherwise, with no handlers beyond `session_start`/`session_switch` until accepted;
one client connection with hello, heartbeat, reconnect and bounded buffer; forward events and snapshots; two
tools registered after hello with schemas and descriptions supplied by the hub (`cataly_lead` for the lead,
`cataly_lane` for lanes; each takes an `op` and typed arguments, so new operations need no bridge release; neither
is named `cataly`, because OMP also offers a tool as a command of its embedded shell, where that name would hide the
CLI from rule 0);
fingerprints and background-job completion facts from the live branch; hooks that exist only inside OMP
(shell veto, subagent-spawn veto, system-prompt append, model barrier, stop gate, tern form interception);
session marker and resumed-transcript ownership; tern control commands. It holds no records, no credentials,
no files and no child processes.

### 10.7 Vetted imports

Owner decision 14 allows Rust the owner wrote in the earlier tree, after the coordinator's per-file check
(`docs/cleanroom.md`, "Vetted imports"). Such files arrive under `import/`. Rules for build agents:

- Before writing a module that `docs/plan.md` marks as an import candidate, look in `import/` and in
  `docs/cleanroom-log.md` for a vetted file covering it. Never look anywhere else for it.
- An import is adapted, never adopted: it is renamed to this glossary, placed in the module named here, made to
  implement this design's traits (`Runtime`, `Relay`, `VoiceHost`, the `omp::rpc` client interface) and to use
  the `store` module for anything durable. Its own persistence, locking, logging or HTTP layers are removed in
  favour of `store`, `log`, `sys` and `relay::http` (decision 13: no second implementation).
- An import that needs a second trait shape, another storage scheme or a dependency outside 10.2 is written
  from the spec instead (reason noted in the clean-room log). Imported modules meet sections 9 and 10.3 and get
  the same tests as any other module.

## 11. Install and update

### 11.1 Single binary

One static executable per target: `x86_64-unknown-linux-musl` (runs on any x64 Linux) and
`aarch64-apple-darwin`. One extension file, the same for both. Release assets: two binaries, `cataly.ts`,
`install.sh`, `SHA256SUMS`, `SHA256SUMS.sig` (IR-02, plus 11.4).

### 11.2 Where files live (same rule on Linux and macOS)

| What | Path |
| --- | --- |
| Program folder | `$XDG_DATA_HOME/cataly` or `~/.local/share/cataly`: `versions/<v>/{cataly, cataly.ts}` (read-only), `current` link, version records, lock, staging |
| Command | `~/.local/bin/cataly` → `current/cataly` |
| Extension link | `<OMP agent dir>/extensions/cataly.ts` → `current/cataly.ts` (agent dir from `PI_CODING_AGENT_DIR` when set, else `~/.omp/agent`) |
| Config and secrets | `$XDG_CONFIG_HOME/cataly` or `~/.config/cataly` |
| State | `$XDG_STATE_HOME/cataly` or `~/.local/state/cataly` |
| Service (optional, explicit command) | systemd user unit / `~/Library/LaunchAgents/` agent |

Installing never starts the hub, writes config, enables a service or touches OMP other than the one extension
link (IR requirement 20). Activation is one atomic rename of `current`, which switches binary and extension
together; running OMP sessions keep the extension they loaded and the hello catches skew.

**npm and Bun installs.** The npm package `cataly` holds only a launcher (`bin/cataly.js`, Node 18+ or Bun)
that runs the binary of the platform package `cataly-<target>` (an optional dependency with `os`/`cpu` set,
so only the matching one installs; no install scripts). That package holds `cataly` and `cataly.ts` side by
side. A binary whose real path is `…/node_modules/cataly-<target>/cataly` is a package install: the hub links
OMP's extension to the package's `cataly.ts` when it starts, the user service runs the package's binary
directly (the path a global upgrade through the same package manager keeps), and `cataly update` hands the
install step to the manager found from the path: npm under `<prefix>/lib/node_modules` (`npm install
--global --prefix <prefix> cataly@<v>`), Bun under `<BUN_INSTALL>/install/global/node_modules` (`bun add
--global cataly@<v>` with that `BUN_INSTALL`). The stop, restart and 30 s wait are the switch's; a new
version that does not come up is replaced by the earlier one the same way. The program folder holds only
the lock, the silenced version and the service marker; there is no rollback and no record. Package installs
rest on the registry's own integrity check, not on the release signature (11.4).

### 11.3 Update check and rollback

- The hub checks GitHub's newest stable Cataly release once per 24 h (first check 10 min after start, 15 s,
  1 MiB) and posts one owner notice per new version, never for a version the owner rolled back from. The
  release repository also holds prereleases and releases of other software: the check reads the list of the
  30 newest releases and takes the highest `v<major>.<minor>.<patch>` that is neither draft nor prerelease and
  carries `SHA256SUMS` and `cataly-<v>.ts`; one that lacks this host's binary or the signature is refused by
  name. `install.sh` makes the same choice. It never downloads or installs by itself (IR-21, IR-22).
- `cataly update [--restart]`, `cataly rollback [--expect V] [--restart]`, `cataly status --install`,
  `cataly check`, `cataly uninstall`. One exclusive lock; distinct exit statuses for done, held, switched back,
  needs attention (IR requirement 34).
- An update or rollback is held while the hub answers on its socket, unless `--restart`: ask the hub to shut down
  in order (it refuses while a session is working or has a pending dialog, and the update stays held after 60 s;
  it never kills busy sessions), switch, start the new hub the way the old one ran, wait 30 s for a status
  answer with the new version, else switch back.
- Rollback keeps exactly one previous version; its two files are re-hashed against its record before switching.
- Store compatibility across versions follows 7.2 (migrations, minimum reader version, backup).

### 11.4 Release verification

- Releases come only from the tag workflow: tag equals `v` + Cargo version, tagged commit is on `main`, full CI
  runs on the tag commit, each target builds twice on its native runner and the two binaries must be identical
  (reproducible build: pinned toolchain, `--locked`, remapped paths, `SOURCE_DATE_EPOCH`, one codegen unit).
- The publish job writes `SHA256SUMS` and signs it with an ed25519 key held as a repository secret; the public
  key is compiled into the binary and printed in `install.sh`.
- `install.sh` (POSIX sh, curl, sha256sum or shasum) checks hashes, then runs the downloaded binary's
  `cataly install --from <dir>`, which re-checks hashes, verifies the signature with the compiled-in key, checks
  the executable format and the candidate's `version --json`, and only then switches.
- From then on every update is verified by the running, already-trusted binary, so only the very first download
  rests on HTTPS alone.
- Until CI runs again, `scripts/release.sh <version>` builds and signs a Linux x64 release on the owner's
  machine (clean tree, Cargo version, `--locked`, `SOURCE_DATE_EPOCH`, key from `~/.config/cataly-release`
  handed to cargo through `env` only) and packs the npm packages with `scripts/npm-pack.sh`. Publishing stays
  a separate, manual step.

## 12. Owner decisions, resolved conflicts and open assumptions

### 12.1 Decided by the owner

From `docs/decisions.md` (2026-11-25). These are no longer assumptions.

| Decision | Topic | What this design does | Replaces row |
| --- | --- | --- | --- |
| 1 | Licence | No upstream code, structure, names or prose; every name in section 1 is designed here; imports only under 10.7 | — |
| 2 | Name | The product, binary, bridge, socket and paths are all `cataly` | — |
| 3 | Relays, voice, tern | Discord, Telegram, Slack (5.1), voice (5.2) and the tern runtime (4.3) are all built | — |
| 4 | Windows | No Windows build or CI; Telegram on Windows dropped | A5 |
| 5 | Lane goals | Lanes use OMP goals over rpc `goal` (verified present in 19.1.1, against HOST req. 65) | C2 |
| 6 | Helper session | The lead is the only standing session; no `duty` role | C5, part of A26 |
| 7 | Model fallback | On: overlay pins `retry.modelFallback: true` and `cooldown-expiry` revert; chains from the owner's config; resets, credits and overage refused in code; model job reports instead of repairing (4.2, 6.1, 9.2) | A1 |
| 8 | Stuck OMP exit | End input, 15 s, SIGTERM, 5 s, SIGKILL, own children only (4.2) | A4 |
| 9 | Hub restart | Every lane that was running is restarted on its exact session file after the lease shows it free. OMP restores a goal paused; the hub sends `goal resume` only for lanes whose goal was active when the hub stopped, and leaves lanes paused by the lead or owner paused (OPC-34) | A7 |
| 10 | Slack senders | Allowlist, default the owner only | part of A20 |
| 11 | Voice transport | WebSocket and WebRTC behind one `RealtimeLink` (5.2); a gated live test against the real API before release, at a cost the owner approves; CI uses a fake | A21 |
| 12 | Updates | Notice only; `cataly update` installs; nothing installs by itself (11.3) | part of A28 |
| 13 | Weight | Two crates, one implementation per concern, dependency list cut (10.1, 10.2) | — |
| 14 | Earlier Rust | Vetted imports under `import/`, adapted to this design's traits and store (10.7; import candidates per milestone in `docs/plan.md`) | — |

### 12.2 Resolved conflicts and open assumptions

"If the owner says otherwise" names what changes. Rows marked **C** resolve a conflict between specs or between
a spec and the installed OMP; rows marked **A** record an answer the design assumed to a spec question. Row
numbers are kept from the first draft so references stay stable; decided rows moved to 12.1.

| # | Question | Default chosen | What changes if the owner says otherwise |
| --- | --- | --- | --- |
| C1 | Control of headless sessions: rpc or bridge (EXT Q1 vs LANE, HOST) | rpc for headless; bridge controls only tern (3.5) | Bridge-based control would add a command channel in `cataly.ts` and a second liveness rule; the `Runtime` trait would stay. |
| C3 | Lead events through the bridge's `sendMessage` (WS req. 5) vs rpc | Inbox in the hub; urgent = rpc wake, quiet waits for the next wake as part of one batch | None at the owner level; only a change of mechanism. |
| C4 | Typing indicator: DROP in DISC-OUT-26, OPC-07, MSG-10; MERGE in OJ-30 | Keep Discord typing driven by turn state (OJ requirement 57); no helper for other relays | Remove one small Discord call. |
| C6 | Quiet-lane nudges: WS-10 (15 min prompt) vs COORD-06 (10 and 20 min reminders, 40 min alert) vs WS-16 stuck | One `oversight` job: COORD timings and texts (the reminder also asks for the progress line), WS-10 gating conditions, WS-16 stuck verdict and WS-18 ceilings beside them | Different timings are configuration only. |
| C7 | Report relay to threads: at once (EVR-19) vs after 20 quiet minutes (WS-11) | Stuck, waiting-on-owner, stopped and finished post at once; progress posts by the quiet rule, at most one per 20 minutes | Policy lists in config. |
| C8 | Owed replies: OPC-14/15 (60 s reminder, any reply closes) vs DISC-OUT-09 (linked reply + read mark, stop gate) | DISC-OUT req. 33 settle rule; both reminders (60 s wake and stop gate) share one cap of 3 and the 30-minute window | Looser settling is one predicate. |
| C9 | Seen mark timing: after hand-off (DISC-IN req. 44) vs on admission (DISC-OUT req. 30, OPC-13, MSG-04) | On durable admission, all routes, in parallel with hand-off | Moving it after confirmation adds latency to the mark only. |
| C10 | Spend-guard setting `codexResets.autoRedeem` (OJ-33) | Does not exist in 19.1.1; the overlay sets `codexResets.autoRedeem5h/Weekly` and `claudeResets.autoRedeem5h/Weekly/Fable` to `no` | Nothing; correction of a name. |
| C11 | Auto-pick of a recommended form answer after 3 min (DISC-OUT-17) vs "never forward timed-out flags" (HOST req. 47) and "silence never authorises" (OPC-10) | Kept for OMP forms with exactly one recommended option: the hub submits that value explicitly (no timed-out flag), says so in the question; never for approvals or confirms | Turning it off makes unattended forms wait until answered. |
| C12 | Hand-off proof: user entry with room and message IDs (DISC-IN req. 48) vs `custom_message` with delivery ID (PATCH-01) | Headless: envelope line in the `user` entry; tern: `details` of a `custom_message` | None. |
| C13 | In-doubt handling differs across DISC-IN req. 50, COORD-20, EVR req. 62, PATCH-01 req. 11 | By kind, as in 9.3 | Owner confirmation for every owner-message resend (DISC-IN Q3) would fence the room instead. |
| C14 | Session cap: HOST-29 (8, range 1-32) vs LANE req. 18 lane count (20) | One cap `max_sessions` = 8 including the lead | Raise the number; memory admission still applies. |
| C15 | Owner notes (COORD-34 to 37) vs owner profile (OPC-79) | One profile file with COORD format, checker, snapshots and OPC-79's evidence rule | Two files if the owner wants them separate. |
| C16 | Request log: lead adds (EVR req. 44) vs hub opens on admission (OPC-80) | Hub opens an entry per admitted request; lead keeps next step and closes | None. |
| C17 | Thread status: lead-set check-mark prefix (DISC-OUT-12) vs hub-set status words from lane state, no emoji (OPC-25, OPC-03) | Hub sets `[working]`, `[waiting]`, `[done]` from lane state and archives on close; the lead may set the same words by hand; `done` refused while a lane is open | A different prefix style is configuration. |
| C18 | Board post per destination (EVR-30) vs one status line per work thread (OPC-06, OPC-25) | One `board` job with two renderers (all lanes, one lane) and one create/edit rule (EVR req. 53) | None. |
| C19 | Extension loading: `-e <path>` (HOST req. 19) vs global link (IR-23, EXT-01) | Global link only; a missing link fails health and start | Passing `-e` with `--no-extensions` would disable the owner's other extensions. |
| C20 | TELE-15 merges into a "recall spec" that does not exist | Telegram turns feed the earlier-conversation store of DISC-IN-18 | None. |
| C21 | Voice spends money; the spend rules of decision 7 | Those rules cover OMP resets, credits and overage; voice is a separate owner-capped budget (VOICE-19) | — |
| C22 | Hub-driven return to the requested model (OJ-11, OJ req. 30-31) vs OMP's own `cooldown-expiry` revert | OMP's revert only; the hub reports (6.1), so one implementation per concern (decision 13) | A hub return path would be a second mechanism that can race OMP's. |
| A2 | Personal-account Discord features (OJ Q2) | Dropped | — (DROP verdicts) |
| A3 | Quota gate scope and level (OJ Q3) | Also holds planner and queued starts; level 85 % | Config. |
| A6 | Lead resume after an unclean stop (HOST Q4) | Resume when the lease shows the transcript free and it exists; else fresh, saying why | Always fresh after a crash. |
| A8 | Tern lifecycle (LANE Q2) | Full lifecycle except remote dialog answers | Restricting tern to open/observe/close shrinks the tern runtime. |
| A9 | Per-lane shell rules (LANE Q3) | Kept, fail closed | Relying on OMP approvals alone removes the shell veto round trip. |
| A10 | Queued launches (LANE Q4), auto close after 5 min (LANE Q7) | Kept; 5-minute window kept | Config. |
| A11 | systemd scopes per lane (LANE Q5) | Not in the first release | Linux-only addition in the `sys` module. |
| A12 | Worktrees without a list (LANE Q6) | No migration exists (IR-43), so unrecorded worktrees are always kept | — |
| A13 | Approval mode for headless sessions (HOST-05) | Owner decision 19 (`docs/decisions.md`): approvals follow the owner's own OMP settings. Sessions start without `--approval-mode` and the overlay sets no `bash.patterns`, so OMP applies the owner's global configuration or its default. Cataly's tools are registered `loadMode: essential` with `policy: allow`, so OMP offers them only as direct calls that never ask; the hub's shell veto refuses a `bash` line naming them, before OMP's approval, with a pointer to the direct call, and rule 0 still refuses `cataly` CLI mutations and spend commands; the overlay denies `eval`. Every dialog OMP does open goes to the lead (LANE-31) and, if enabled, to the owner as a card in the owner's language with owner-only buttons. Review lanes in print mode keep `always-ask`: nobody can answer there, so it fails closed. | Forcing `always-ask` overrode the owner's own OMP choice and made every write ask in Discord; a Cataly read-only list duplicated OMP's own approval configuration. |
| A14 | OMP's automatic rotation between the owner's own logged-in accounts | Not controlled by Cataly (no documented switch); allowed like a configured fallback | If the owner counts it as "switching account", keep one login per provider. |
| A15 | Discord: all-channels mode, lookup, auto resend, busy lead, rejected intents (DISC-IN Q1-5) | Dropped; lookup opt-in and off; auto resend when the file is final; follow-up while busy; intake stays down | Each is a small switch except REST polling, which stays out. |
| A16 | Hub posts final answers (DISC-OUT Q1) | Yes, lead and lanes | Model-driven replies need the stop-gate machinery as the main path. |
| A17 | Form timing (DISC-OUT Q2) | 3 minutes for a single recommended choice; no timeout otherwise | Config. |
| A18 | Remote approvals by button (DISC-OUT Q3) | Built, off by default, fail closed | Removing it deletes one card type. |
| A19 | Owner language, table rendering (DISC-OUT Q4, Q5) | One configured language; tables always rendered | Config. |
| A20 | Slack member mode, Telegram send scope (TSV Q1, Q4) | Member via the app's user token; Telegram sends limited to the owner's DM and configured group/topic | Config. |
| A22 | Voice-note transcription (TSV Q5) | On request; automatic is a config switch | Automatic transcription adds a call per voice note. |
| A23 | Ceilings hold the lane (WS Q1) | Yes, plus `--max-cost` as OMP-side backstop; also bounds metered fallback spend | Advisory only. |
| A24 | Auto report posting, PR events to threads, limit alerts, broken-off cron (WS Q2, Q4-6) | As C7; PR events to the lead only; quota job alerts on bands; broken-off jobs stay held until acknowledged | Config. |
| A25 | Personal watch scripts (WS Q3) | Covered by the job kinds of 6.1 plus `probe` and `stream` | A new kind if a script needs carried state. |
| A26 | Coordination questions (COORD Q2-5) | Usage counts built but off and with no default collector; event commands kept; planner defaults kept; profile kept | Usage counts can be removed entirely. |
| A27 | Evidence questions (EVR Q1-5) | Published files kept; progress reports do not wake; PR policy check kept; caps as specified; request log kept | Each is one feature flag or one constant. |
| A28 | Release questions (IR Q1, Q3-5, Q7, Q8) | Signed checksum list; `--restart` never stops busy sessions; static musl; XDG paths on macOS; no OMP skill file (guidance comes from the hub); no notarisation | Unsigned releases drop 11.4's signature step; notarisation adds a macOS signing job. |
| A29 | Operating contract (OPC Q1-5) | Short neutral voice; the hub only wakes the lead after 60 s, never posts a holding line itself; lead may settle routine reversible choices; onboarding reads titles and metadata unless allowed more; profile distillation kept, simplified | Each changes guidance text or one rule. |
| A30 | OMP patch questions (PATCH Q1-3) | Detect and report misdelivery in tern; file the four upstream reports; minimum 19.1.1, newer majors allowed with an "untested" warning | Refusing untested majors adds an upper bound to the probe. |
| A31 | EXT Q2, Q4, Q5 | Form mirroring kept for tern; restricted second lead dropped; lane model pinned for its life, OMP-announced fallbacks excepted (6.1) | Config. |
| A32 | Who may call the hub's owner verbs from a shell | CLI = owner unless the peer descends from a hub-spawned session; lanes' shell calls to `cataly` are vetoed except read-only verbs (8) | A stricter scheme would need a second OS user for sessions. |
| A33 | Provider standby for voice (rest of VOICE-12) | Not built; idle close plus wake on speech | A standby link adds one idle connection per call window. |
