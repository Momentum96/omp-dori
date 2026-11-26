# Build plan

Ordered milestones. Each is a vertical slice: it ends in something the owner can run, ships with tests on
linux-x64 and darwin-arm64, and records the measures named in it against `DESIGN.md` section 2.3. A milestone
is done only when its acceptance checks pass in CI, including the contract suite against stock OMP 19.1.1 and
the newest OMP release. Owner decisions in `docs/decisions.md` override anything here.

Capability IDs are the spec IDs. Each KEEP, SIMPLIFY or MERGE capability is listed under exactly one milestone,
the one that first delivers it; a later milestone that extends it says so in its text. MERGE capabilities are
built as part of the capability they merge into, in the same milestone. The product is two crates,
`cataly` and `cataly-voice` (`DESIGN.md` section 10.1); "modules touched" names modules of `cataly` unless
stated.

**Import candidates** (owner decision 14, `DESIGN.md` section 10.7). Each milestone lists the modules for which
the owner may supply his own earlier Rust. Before writing such a module from the spec, the build agent checks
`import/` and `docs/cleanroom-log.md` for a vetted file covering it; if there is one, it adapts that file to
this design's names, traits (`Runtime`, `Relay`, `VoiceHost`, the `omp::rpc` interface) and `store`, and
removes any layer the design already has once. If there is none, or the file would need a second trait shape
or storage scheme, the module is written from the spec. Nothing about an import's shape is assumed in advance;
the three the owner named as likely are marked **likely**. The same check applies to any other module if a
vetted file for it appears in `import/`.

Rules for every milestone:

- Memory and latency benchmarks run in CI and are compared with `main`; a rise over 10 % blocks the merge.
- `docs/third-party.md` lists every new dependency with licence and source before it lands.
- No capability is shipped behind a switch that is off and untested; anything configurable is tested both ways.
- `docs/cleanroom-log.md` gets one line per milestone: who built it, what they were allowed to read, and which
  vetted imports (if any) it adapted.
- No module duplicates one listed in `DESIGN.md` 10.1 "One implementation per concern"; review rejects a second
  HTTP client, session-file reader, subprocess runner, inbox or transition memory.

## Milestone 1: hub skeleton, job engine, owner jobs

The owner runs `cataly hub` with the GitHub watch, quota watch, backup, health, mail watch and timed jobs,
reads events with `cataly events`, and receives job posts in a Discord channel. No OMP session yet.

**Scope**

- Hub process, lock, supervision, control socket and CLI: HOST-22, HOST-36, HOST-37, WS-32, COORD-43,
  COORD-53.
- Configuration and setup: WS-35, COORD-51, COORD-52, IR-09, OPC-90.
- OMP probe and diagnostics (no sessions): HOST-03, HOST-35, PATCH-09, PATCH-13.
- Job engine and typed events: WS-01, WS-03, WS-28, WS-29 (local, lead backlog, Discord destinations),
  WS-33 (start-up summary, job and health lines), WS-34.
- Timed and condition jobs: WS-26, WS-27, WS-30. Stream jobs run here from the CLI; the lead's tool for them
  comes in milestone 2.
- Pull requests and account limits: WS-23, WS-24, WS-25.
- Owner jobs: OJ-01, OJ-02, OJ-03, OJ-04, OJ-05, OJ-06, OJ-07, OJ-08 (the gate flag and its lifting; it
  starts holding session starts in milestone 2), OJ-09, OJ-15, OJ-16 (job, file-age and messenger checks;
  lead checks added in milestone 2), OJ-18, OJ-19, OJ-26.
- Host sampler and pressure notices: LANE-08 (sampler and the admission rule as a library), LANE-09, WS-36.
- Discord posting for job destinations: DISC-OUT-01, DISC-OUT-03, DISC-OUT-06, DISC-OUT-23, DISC-OUT-24,
  DISC-OUT-27; shared send model and HTTP policy MSG-05, MSG-06.
- Contract suite skeleton and CI: PATCH-12 (probe cases), IR-36, IR-38 (pinned and nightly OMP), IR-39.

**Modules touched:** `cli`, `hub`, `proto`, `sys`, `log`, `store`, `omp` (`rpc` for the probe, usage and lease
parsers), `jobs`, `relay` (`http`, `text`, Discord REST only); `tests/support/` (fake `gh`, fake Discord REST).

**Import candidates:** `omp::rpc` (framing, v2 chunks, correlation) **likely**; `relay::discord` REST client
and `relay::http` **likely**.

**Acceptance checks**

1. A second `cataly hub` exits at once naming the running one; a stale socket file is replaced.
2. A connection from another UID is closed without reply (test runs a helper under a second user in CI on
   Linux; macOS covered by a unit test of the credential check).
3. `cataly setup` creates the config with preview, refuses a file changed since its snapshot, and reports each
   managed setting's source.
4. An invalid config (unknown key, cadence below minimum, unknown target, template naming an unknown field) is
   refused with the setting path and the previous config stays in force.
5. Cron: DST gap does not fire, DST repeat fires once, 60 s late is skipped and counted, runs never overlap;
   one-shot is consumed before it runs; condition jobs fire on change to success and skip identical output.
6. A crash injected between event append and state commit repeats the event after restart and loses none.
7. GitHub watch against a fake `gh`: baseline reports nothing old; a new PR, a review by someone else and a
   failed check each produce one event; own and bot comments do not.
8. Quota job against fixture `omp usage --json` outputs: band rises warn once, the gate closes at 85 % and opens
   at the reported reset time even when data stays unavailable.
9. Backup refuses a staged GitHub token and a 60 MiB file, names files and rule but not the secret, re-alerts
   only when the file set changes, never force-pushes.
10. Health reports a problem only after two bad ticks and its clearing once; the stall detector restarts the
    hub under systemd and launchd in CI.
11. Job posts to a fake Discord carry no mentions, an enforced nonce, and are refused by the outgoing text
    checks when they contain `{unfilled}` or a banned character.
12. The probe against OMP 19.1.1 reports version, protocol 1/2, frame limits and the feature set; against a
    fake `omp` printing 19.0.0 no session start is possible and health names both versions.

**Measured:** hub idle RSS with all owner jobs (target ≤ 30 MiB total, jobs < 5 MiB); hub start to socket
(≤ 150 ms); `cataly status` round trip (≤ 20 ms); CPU time per hour with all jobs idle; release binary size;
the reqwest-versus-hyper comparison of `DESIGN.md` 10.2 (binary size and RSS; reqwest is replaced if hyper
saves over 1 MiB); incremental debug build time of `cataly` (split trigger at 30 s, `DESIGN.md` 10.1).

## Milestone 2: Discord text end to end with one headless lead

The owner writes in a Discord channel, the lead session wakes, answers, and the hub posts the answer as a
linked reply. Approvals and OMP forms can be answered from Discord. Model watch, spend guard, digest and
unanswered-message watch run.

**Scope**

- Headless session host: HOST-01, HOST-02, HOST-04, HOST-05, HOST-06, HOST-07, HOST-08, HOST-09, HOST-10,
  HOST-11, HOST-12, HOST-13, HOST-14, HOST-15, HOST-16, HOST-17, HOST-18, HOST-20, HOST-21, HOST-23,
  HOST-24, HOST-26, HOST-28, HOST-29.
- Deliveries, inbox and confirmation from session files: PATCH-01 (headless rules 1-5, 8-11, 15-17; tern
  rules in milestone 6), COORD-01, COORD-02, COORD-10, COORD-12, COORD-13, COORD-15, COORD-18.
- Bridge, lead side: EXT-01, EXT-02, EXT-03, EXT-06, EXT-08, EXT-09, EXT-10, EXT-11, EXT-12, EXT-17,
  EXT-23, EXT-24, EXT-33, EXT-35, EXT-36, EXT-37; lead's job tool WS-02, WS-04; WS-09, WS-40.
- Discord intake: DISC-IN-01, DISC-IN-02, DISC-IN-04, DISC-IN-05, DISC-IN-06, DISC-IN-07, DISC-IN-08,
  DISC-IN-11, DISC-IN-12, DISC-IN-13, DISC-IN-15, DISC-IN-16, DISC-IN-19, DISC-IN-20, DISC-IN-21,
  DISC-IN-23, DISC-IN-24, DISC-IN-25; shared messenger model MSG-01, MSG-02, MSG-03, MSG-04, MSG-11.
- Discord outbound: DISC-OUT-02, DISC-OUT-04, DISC-OUT-07, DISC-OUT-08, DISC-OUT-09, DISC-OUT-10,
  DISC-OUT-15, DISC-OUT-16, DISC-OUT-17, DISC-OUT-18, DISC-OUT-20, DISC-OUT-21, DISC-OUT-22, DISC-OUT-28.
- Owner jobs that need sessions or intake: OJ-10, OJ-11, OJ-12, OJ-13, OJ-20, OJ-21, OJ-22, OJ-25, OJ-27,
  OJ-29, OJ-30, OJ-31. Quota gate now holds the lead's start. Model fallback is on (decision 7): OJ-11 is
  delivered as the overlay's pinned `cooldown-expiry` revert plus reporting, not as a hub return path.
- Lead records: EVR-22, EVR-23, EVR-24, EVR-25, EVR-28, EVR-35, EVR-36.
- Operating contract, lead part (guidance text plus the code rules): OPC-01, OPC-02, OPC-03, OPC-04,
  OPC-05, OPC-08, OPC-09, OPC-10, OPC-11, OPC-12, OPC-13, OPC-14, OPC-15, OPC-16, OPC-29, OPC-30, OPC-36,
  OPC-50, OPC-51, OPC-52, OPC-53, OPC-55, OPC-63, OPC-64, OPC-65, OPC-74, OPC-75, OPC-76, OPC-77, OPC-78,
  OPC-80, OPC-81, OPC-83, OPC-84, OPC-85, OPC-86, OPC-87, OPC-89.
- Contract suite: rpc cases of PATCH-12 (prompt, follow-up, steer, abort, `prompt_result`,
  `session_settled`, dialogs, resume identity, lease) and bridge load and inert behaviour.

**Modules touched:** `omp` (`rpc`, launch, overlay, `transcript`), `host` (headless runtime, actors, `inbox`,
receipts, dialogs), `relay` (pipeline, Discord Gateway, intake, forms, buttons), `jobs` (model, spend, digest,
unanswered), `store`; `extension/cataly.ts`; `tests/support/` (fake Gateway, fake model provider).

**Import candidates:** `host` headless session runtime **likely**; `relay::discord` Gateway client **likely**;
`omp::rpc` if not already adapted in milestone 1; `omp::transcript`.

**Acceptance checks**

1. A Discord owner message to an idle lead produces `agent_start`, the final answer is posted as a confirmed
   linked reply in parts with deterministic nonces, the 👀 mark is added, and the owed reply settles.
2. While the lead is busy, a second message is queued as a follow-up and answered after the first.
3. Non-owner messages, bot messages, webhooks, edits and deletions are never delivered and still advance the
   read position; a reply parent written by someone else reaches the lead as data.
4. Kill the hub between arrival-queue commit and hand-off: on restart the catch-up read and queue deliver the
   message exactly once. Kill after the prompt is written but before confirmation: the file search confirms it;
   with the OMP process gone and no entry, it is resent once; otherwise the room is fenced and the owner told.
5. Gateway faults from the fake server (op 7, op 9 both ways, close 4004, 4008, missed heartbeat ACK, a bad
   resume URL) follow DISC-IN requirements 18-21.
6. A transcript switch reported by `get_state`, a model mismatch at start, and an `rpc_frame_error` each fence
   the session; nothing is delivered afterwards.
7. An OMP form posts a question with buttons; a button press by the owner answers it; a press by someone else
   gets an ephemeral refusal; a single recommended option is submitted after 3 minutes; the local answer and the
   Discord answer race correctly.
8. A spend-offer dialog (fake model triggering a `select` with "redeem") is cancelled within 1 s and reported;
   three in 10 minutes fence the session.
9. With a fallback chain configured, a usage-limit error from the fake provider moves the lead to the chain's
   next model: one quiet fallback event, the overlay shows `retry.modelFallback: true`, no reset is redeemed and
   no saved-reset command runs; when OMP restores the primary, one recovery note; with no chain configured the
   turn ends quota-blocked and the lead is told once. A `usageLimit` with `extraUsage` fences the session.
10. Owner return after 3 hours produces a digest delivered to the lead as one urgent event.
11. Bridge in an ordinary OMP session registers only start/switch handlers and opens no connection.
12. Restart: the lead resumes its last cleanly closed session (same ID and file); after an unclean stop with a
    free lease it resumes with a note; with a held lease it is reported and not started.

**Measured:** wake latency, hub part and end to end (DESIGN 2.3); hand-off confirmation time (prompt written to
entry found); RSS with the lead attached; bridge start-up cost in an ordinary session (< 5 ms); Gateway
reconnect to catch-up complete.

## Milestone 3: lanes

The lead opens lanes from Discord requests, each in its own thread; lanes report, claim, are paused, parked,
closed and reopened; oversight nudges quiet lanes; the planner starts backlog items. Delivered in two slices,
each releasable.

### Slice 3a: lane lifecycle, reports, oversight

**Scope**

- Lifecycle: LANE-01, LANE-02, LANE-04, LANE-05, LANE-06, LANE-07, LANE-13, LANE-14, LANE-15, LANE-16,
  LANE-17, LANE-18, LANE-19, LANE-20, LANE-21, LANE-22, LANE-23, LANE-24, LANE-25, LANE-26, LANE-27,
  LANE-28, LANE-30, LANE-31, LANE-33; goals over rpc HOST-19.
- Bridge, lane side: EXT-04, EXT-14, EXT-15, EXT-16, EXT-27, EXT-31, EXT-32.
- Reports, evidence, done checks, lead-to-lane messages, board: EVR-01, EVR-02, EVR-04, EVR-05, EVR-06,
  EVR-07, EVR-08, EVR-09, EVR-11, EVR-12, EVR-14, EVR-15, EVR-16, EVR-20, EVR-21, EVR-29, EVR-32, EVR-33,
  EVR-34, EVR-38, EVR-39, EVR-40.
- Oversight and spend: WS-10, WS-11, WS-13, WS-14, WS-15, WS-16, WS-17, WS-18, WS-20, WS-21, WS-22, WS-38,
  WS-39; COORD-06, COORD-07, COORD-08, COORD-09, COORD-42, COORD-44, COORD-45.
- Threads and routing to lanes: DISC-IN-14, DISC-IN-22, DISC-OUT-11, DISC-OUT-12, DISC-OUT-13, OJ-14,
  OJ-28.
- Addresses and targets: COORD-16, COORD-46, COORD-47, COORD-48, COORD-49.
- Operating contract, lane part: OPC-06, OPC-17, OPC-18, OPC-19, OPC-20, OPC-21, OPC-23, OPC-24, OPC-25,
  OPC-26, OPC-27, OPC-28, OPC-31, OPC-32, OPC-33, OPC-34, OPC-35, OPC-37, OPC-38, OPC-39, OPC-40, OPC-41,
  OPC-42, OPC-43, OPC-44, OPC-45, OPC-46, OPC-47, OPC-48, OPC-49, OPC-54, OPC-56, OPC-57, OPC-60, OPC-61,
  OPC-62, OPC-82, OPC-93.

**Modules touched:** `lanes` (new module), `host` (goals, lane roles, look-only tool sets), `jobs`
(oversight, board), `relay` (threads, journal); `extension/cataly.ts` (lane tool, shell veto, subagent veto,
model barrier with fallback exception, fingerprints).

**Import candidates:** none named by the owner; check `import/` for lane or git worktree code before writing
`lanes`.

**Acceptance checks**

1. Open with preview writes nothing; open refuses a bad key, an unparseable done-check list, a model alias and a
   reserved word; request-ID dedupe returns the earlier result.
2. A model mismatch at start keeps the process, sends no brief, marks "start held: model" and tells the lead.
3. Report authority: a report from a closed lane, an earlier run or another session is refused with "the lane
   moved on"; evidence novelty ignores a restated report and an identical file.
4. Claim, objection window and automatic close: unsent git work and a failing check turn into an objection; a
   crash after the close intent resumes the close from its step.
5. Pause confirms the goal paused before interrupting; resume restores state; a paused lane that starts a turn
   stays paused and the lead hears once per active stretch.
6. Park, then reopen on the exact session file with a new run ID; a header naming another session refuses.
7. Shell rules: a denied command never runs, the block names rule numbers and a digest only; a one-attempt grant
   is used up by the next identical command; a bridge disconnect blocks every shell call.
8. Oversight: reminders at 10 and 20 quiet minutes only while idle, one stall alert at 40 minutes, stuck verdict
   only with a complete sample; reaching a cost ceiling pauses the lane and `--max-cost` is on its command line.
9. A lane's thread receives stuck/finished reports at once and progress reports by the quiet rule; paths are
   cleaned; each report posts at most once.
10. Hub restart with three running lanes, two with active goals and one paused by the lead (decision 9): all
    three are restarted on their exact files once the lease is free; the two get `goal resume` and read back
    active; the paused one stays paused; the lead receives the recap.

**Measured:** lane start time (spawn to kickoff confirmed); RSS added per lane; oversight CPU per hour with 8
lanes; incremental spend reading cost per 10 MiB of session file.

### Slice 3b: coordination

**Scope**

- Slots and queued launches: LANE-10, LANE-11, COORD-31, COORD-33.
- Planner and backlog: COORD-19, COORD-20, COORD-21, COORD-22, COORD-23, COORD-25, COORD-26, COORD-27,
  COORD-28, COORD-29, COORD-30.
- Profile: COORD-34, COORD-35, COORD-36, COORD-37, OPC-79.
- Review and merge gate: COORD-38, COORD-39, COORD-40, OPC-58, OPC-59.
- Event commands: COORD-41. Admission command and slots view: WS-37.
- Published files, learned rules, board posts: EVR-17, EVR-19, EVR-27, EVR-30.

**Modules touched:** `lanes`, `jobs` (planner job, board posts), `cli` (slot command).

**Import candidates:** none named; check `import/` before writing `lanes` slots and planner.

**Acceptance checks**

1. `cataly slot build -- <cmd>` waits by priority then arrival, passes the exit status through, keeps the slot
   until both client and command exit, and grants nothing for 5 s after a hub restart.
2. A queued launch becomes ready only when every predecessor closed completed with passing checks, and is never
   started by the hub itself.
3. Planner: two failed starts turn an item into a question; an item whose lane claimed with a merged PR becomes
   done; the gate and lane maximum limit starts; the quota gate holds planner starts.
4. Merge gate refuses without an approving verdict at the current head, with no checks, or with any check
   neither passed nor skipped; a dry run stops before merging.
5. The profile check fails on a lost decision, a future date or a token-shaped string; revert saves a backup.
6. A board post is created once, edited on change at most once a minute, and never recreated after an
   uncertain create.

**Measured:** planner pass duration with 100 items; RSS with planner and slots active.

## Milestone 4: Telegram and Slack

The owner talks to the lead from Telegram (DM or one forum topic) and Slack (app or member), with seen marks,
answers in place, job and board destinations on both.

**Scope**

- Shared model extended to all relays (MSG-01 to MSG-06, MSG-11 delivered in milestones 1-2 for Discord).
- Telegram: TELE-01, TELE-02, TELE-03, TELE-04, TELE-05, TELE-06, TELE-07, TELE-08, TELE-09, TELE-10,
  TELE-15.
- Slack: SLACK-01, SLACK-02, SLACK-03, SLACK-04, SLACK-05, SLACK-06, SLACK-07, SLACK-08, SLACK-09,
  SLACK-10, SLACK-12, DISC-OUT-14.
- Channel facts and earlier-conversation lookup for all relays: DISC-IN-17, DISC-IN-18.
- Setup guidance: OPC-88.

**Modules touched:** `relay` (`telegram`, `slack`), `jobs` (destinations); `tests/support/` (fake Telegram and
Slack servers).

**Import candidates:** `relay::telegram` and `relay::slack` clients, only if the owner's earlier Rust has them.

**Acceptance checks**

1. Telegram refuses to start with a webhook set or a non-bot identity; 409 shows unhealthy and retries every
   30 s; a 429 of any length is waited out; the 24-hour offset rule and replay guard hold across restarts.
2. Live-location edits are dropped; topic and General are told apart; sends outside the owner's DM and
   configured group/topic are refused before any request.
3. Slack: an untagged reply in a thread the account posted in is admitted; an unlisted member is dropped; an
   envelope is acknowledged within 3 s; an upload URL off `files.slack.com` is refused before bytes are sent.
4. One stalled relay (fake server hangs) never delays another relay's intake.
5. A Telegram and a Slack request each get the lead's answer in place with a seen mark.

**Measured:** intake latency per relay (frame or poll response to `agent_start`); Telegram receive-lag
percentiles; RSS with three relays connected (target unchanged: ≤ 30 MiB idle).

## Milestone 5: voice

The owner joins a Discord voice channel, talks to a realtime model that can read status, search, leave a note
and pass work to the lead, and hears the lead's replies, within configured spending caps.

**Scope:** VOICE-01, VOICE-02, VOICE-03, VOICE-04, VOICE-05, VOICE-06, VOICE-07, VOICE-08, VOICE-09,
VOICE-10, VOICE-11, VOICE-12 (WebRTC transport only, decision 11; provider standby stays out), VOICE-13,
VOICE-14, VOICE-15, VOICE-16, VOICE-17, VOICE-18, VOICE-19, VOICE-20, VOICE-21, VOICE-22, VOICE-24, OJ-32.

**Modules touched:** crate `cataly-voice` (new, feature `voice`: Discord voice, DAVE, Opus, `RealtimeLink`
with WebSocket and WebRTC); in `cataly`: `relay::discord` (voice-state intent on the shared Gateway), `host`
(`VoiceHost` implementation: lead delivery with receipt proof); `tests/support/` (recorded audio, fake
realtime server for both transports).

**Import candidates:** Discord voice connection, Opus and realtime-session code, only if the owner's earlier
Rust has them; they must fit `VoiceHost` and `RealtimeLink`.

**Acceptance checks**

1. Voice stays off with any required setting missing and names the field; it never starts while a cap is
   reached; a corrupt ledger stops voice and is never reset.
2. Only the owner's audio is received; a dead receive stream is resubscribed; unencrypted frames are accepted
   only within 30 s of an encryption change.
3. Barge-in clears playback; wake on speech sends the 2.5 s lead-in; input is paced at 1.15× real time.
4. Pass-to-lead reports exactly one of three outcomes; "receipt unproven" is never resent.
5. A lead reply counts as spoken only after acknowledgement, transcript coverage and drained playback in the
   same generation.
6. Any provider failure holds voice until an explicit restart, with no paid retry; the summary post goes out.
7. `cataly transcribe <file|url>` runs the owner's command with the 25 MiB cap and container check.
8. Both transports pass checks 3 to 6 against the fake realtime server in CI; switching the transport in config
   changes nothing else in behaviour, ledger or receipts.
9. **Live test, owner-gated (decision 11).** `tests/live_voice` runs against the real realtime API over WebSocket
   and over WebRTC: connect, one exchange from recorded audio, one pass-to-lead with receipt, and a stop at a
   deliberately low spending cap. It is ignored by default and never runs in CI. It runs only when the owner
   puts the API key in the environment and sets an approved spending limit (for example
   `CATALY_LIVE_VOICE_MAX_USD`); the test checks its own ledger against that limit before each call and
   reports the cost it used. It must pass before the first release that contains voice, and again before any
   release that changes `cataly-voice`.

**Measured:** RSS during a call (≤ 25 MiB added) for each transport; CPU during a call; speech end to first
audio out for each transport; binary size with and without the `voice` feature.

## Milestone 6: terminal runtime

The owner can choose Tern for new sessions and watch the lead or a lane work in a Tern block, with the same
control from Discord.

**Scope:** HOST-30, HOST-31; EXT-20, EXT-21, EXT-25, EXT-26, EXT-30; COORD-14; the tern rules of PATCH-01
(6, 7, 12, 13, 14).

**Modules touched:** `host` (tern runtime), `extension/cataly.ts` (control commands, switch guard, form
interception); `tests/support/` (Tern stand-in that runs the command line in a pty).

**Import candidates:** none named; check `import/` for a tern runtime before writing it.

**Acceptance checks**

1. A tern start opens a block running OMP; the bridge hello presents the single-use token; a nested OMP inherits
   the used token and is told "none".
2. Deliver (wake, steer), interrupt, model change, goal pause/resume and shutdown work through the bridge; a
   delivery during a user's session switch is refused "not current" and kept pending.
3. A misdelivery (delivery ID found in another session's file) is never resent and the owner is told once.
4. A dialog in a tern session marks it blocked and is never answered by the hub; a mirrored form can be
   answered from Discord and the local dialog is withdrawn.
5. Losing the bridge for 45 s marks the lane lost; the same process reconnecting with the same session ID
   restores it.
6. With `tern` configured and the executable missing, starts are refused with an install hint.

**Measured:** tern wake latency (relay frame to `agent_start` seen through the bridge); bridge memory and CPU
in an active tern session.

## Milestone 7: installer, release and first run

The owner installs Cataly with one command, gets update notices, updates and rolls back safely; the lead runs
onboarding on first contact.

**Scope**

- Install, update, rollback, status, uninstall, service: IR-01, IR-02, IR-06, IR-07, IR-12, IR-13, IR-15,
  IR-16, IR-17, IR-18, IR-19, IR-20, IR-23, IR-24, IR-27, IR-43.
- Update notice: IR-21, IR-22.
- Release workflow and supply chain: IR-30, IR-31, IR-32, IR-34, IR-35, IR-40.
- Onboarding and first run: OPC-66, OPC-67, OPC-68, OPC-69, OPC-70, OPC-72, OPC-73.
- Not code: file the upstream reports for PATCH-02, PATCH-04, PATCH-05 and the expected-session option of
  PATCH-01 with OMP's maintainers, if the owner agrees (DESIGN section 12.2, row A30).

**Modules touched:** `cli` (install commands), `sys` (links, locks, service files), `jobs` (update job),
release workflow files, `install.sh`.

**Import candidates:** install, link-switch and update code, only if the owner's earlier Rust has it and it fits
`DESIGN.md` section 11 (two files, one link rename).

**Acceptance checks**

1. Fresh install from a local folder through `install.sh`: layout, modes, command link and extension link as in
   DESIGN 11.2; reinstall of the same version changes nothing.
2. Update to a higher build and rollback, status clean after each; an interrupted switch (killed between rename
   and record) is repaired by status.
3. Refused with nothing changed: tampered binary, tampered extension, missing checksum line, bad signature,
   wrong-target binary, foreign file at a link path, update while a test hub answers.
4. `--restart` switches back when the new hub does not report healthy within 30 s, and holds (never kills) when a
   session is busy.
5. Two builds of the same commit on each runner are byte-identical.
6. Upgrade from the previous published release reads its config and state (store migration with backup).
7. The update job announces a newer stable release once, never a rolled-back version, and makes no request when
   switched off.
8. Onboarding asks one consent question before reading anything; a "no" is stored and not asked again; messenger
   writes from the lead are refused while onboarding runs.

**Measured:** install time on each target; binary size per target; update with `--restart` downtime (socket
closed to new hub answering).

## Capabilities not placed

None. Every KEEP, SIMPLIFY and MERGE capability of the 13 specs is listed above, as are the REPLACE-PUBLIC
and MOVE-OUT items of `spec/omp-patch.md`. The UPSTREAM items (PATCH-02, PATCH-04, PATCH-05) carry no code
and appear in milestone 7 as reports to file. DROP capabilities are not built, with one owner exception:
VOICE-12 (DROP in the spec) is built in milestone 5 for its WebRTC transport only, by decision 11.
