# Build notes: milestone 1

Choices made where `DESIGN.md` is silent, and deviations from it, with reasons.

## Layout and names

- `config` is a top-level module (not listed in DESIGN 10.1): the CLI (`config check`), the hub (load,
  reload) and the job engine all use it; putting it under `hub` would make `cli` and `jobs` depend on `hub`.
- Exit statuses (`cli::exit`): 0 ok, 1 refused/failed, 2 command line refused, 3 no hub, 4 hub already
  running, 70 internal (supervision gave up, store writer failed, stall).
- Paths: config `$XDG_CONFIG_HOME/cataly/config.toml`, state `$XDG_STATE_HOME/cataly` (`cataly.db`,
  `hub.lock`, `logs/hub.log`, `logs/events.jsonl`, `files/runs/<job>/`, `backup/`). Socket:
  `$XDG_RUNTIME_DIR/cataly/hub.sock` when set and the path fits the platform limit, else
  `/tmp/cataly-<uid>/hub.sock` (the macOS rule of DESIGN 8, applied on both platforms). Only the XDG
  variables override paths; tests use them with a short root under `/tmp`.
- The hub log goes to `<state>/logs/hub.log` (rotated at 10 MiB to `.1`); start-up refusals also go to
  stderr. The event log rotates at 5 MiB to `.1`, so the last 5 to 10 MiB stay readable.

## Config

- Shape: `[hub] log_level`; `[destinations.<name>] kind = "command", argv = [...]`;
  `[jobs.<name>] kind = cron|once|probe`, `argv`, `cwd` (absolute), `timeout` (default 30s, 1s..1d),
  `enabled`, `dry_run`, `destinations` (names; `log` is the built-in keep-locally destination),
  `templates` (event kind -> text with `{field}`); cron: `cron`, `zone`; once: `at` (instant with seconds
  and offset); probe: `every` (30s..1d), `fire = change|every`, `skip_identical`.
- Durations are strings (`30s`, `5m`, `1h30m`, `1d`).
- The config file must be owner-only (DESIGN 7.2 "loose modes are refused"), so a 0644 file is refused
  with the chmod hint. A missing file means defaults; a hub refuses to start on an invalid file.
- Reload: `cataly config reload` (method `config.reload`) and SIGHUP. `config reload` is a verb beyond the
  assignment's list; WS req. 82 needs a way to ask for the reload.
- A one-shot whose `at` is already past when the hub first sees it is consumed and counted as skipped
  (WS req. 53 "future at creation" applies to config files this way); `config check` warns when an `at`
  is under two minutes away.
- `job enable`/`job disable` are hold/resume (WS req. 66) stored in the `jobs` table; `enabled = false` in
  the config is separate and shown as `disabled`.

## Store

- Event deliveries are receipts in `event_deliveries` (states waiting, attempting, sent, failed,
  uncertain). An attempt is marked `attempting` durably before it starts; at start-up `attempting`
  becomes `uncertain` and is never repeated. `waiting` deliveries that were never attempted are still
  delivered (stricter than WS req. 67, which would make them uncertain too).
- Order of a run: run record (one-shots are consumed in this same transaction), spawn, run end, event
  append, then the state commit (memory, run `applied`). A run that finished but was not applied is
  applied again at start-up, so its event repeats (acceptance check 6); a run still `running` at start-up
  is broken off and its job held until `job enable --ack` (WS req. 67). A graceful stop sends TERM/KILL to
  job children and records their runs as broken off the same way.
- Retention (daily, first run 10 min after start): events older than 7 days beyond the newest 1,000; job
  runs older than 30 days except each job's newest, with their output files. Condition jobs delete the
  output of a success that did not fire, and keep only the latest failing run's output.
- Readers: a pool of two read-only connections used through `spawn_blocking`.
- The first migration is marked non-additive, so `meta.min_reader = 1`.
- Crash test: a `#[cfg(test)]` fault point (`CATALY_FAULT=after_event_append`) aborts a child copy of the
  unit-test binary between event append and state commit; the parent reopens the store and recovers.

## Scheduler and jobs

- Each sleep is capped at 60 s so a wall-clock jump is noticed within a minute (DESIGN 6.2 says "no
  polling loop"; this is one wake per minute at most when idle, not a poll of job state).
- DST: a repeated local time fires once, at its first (earlier-offset) occurrence; every repeated minute
  of the second pass is skipped too. Day-of-month and day-of-week combine with OR only when both are
  restricted (a field starting with `*` is unrestricted), the usual cron reading of WS req. 52.
- A probe keeps its stored due time while it runs, so a crash mid-run probes again after acknowledgement;
  the next due time is written when it finishes.
- Event kinds: `hub.started`, `hub.stalled`, `config.invalid`, `config.valid`, `run.done`, `run.skipped`,
  `run.broken`, `probe.fired`, `probe.error`, `probe.recovered`. Urgency: run results, fired probes, broken
  runs and an invalid config are urgent; the rest quiet.
- Job commands get an empty environment plus PATH, HOME, USER, LOGNAME, LANG, LC_ALL, TMPDIR, TZ and
  `CATALY_JOB`, `CATALY_DRY`. Command destinations get `CATALY_EVENT` (the event as JSON), 30 s, 4 at a
  time; a time-out is recorded as uncertain.
- Authority: the dispatcher has one `Authority::Owner` in this slice; session roles arrive with the bridge.

## Liveness

- The stall detector ends the process (status 70, marker file `<state>/stall-exit`) only when no systemd
  watchdog is armed; under systemd it stops pinging and lets systemd act. The next start emits
  `hub.stalled` once.
- Signal handlers are installed before the socket answers, so an early TERM is still graceful.

## Deviations from DESIGN.md

- `tokio` `macros` feature added (10.2 lists none): `select!` is used throughout; it is compile-time only.
- `tokio` `fs` feature not enabled: nothing needs it yet.
- `rustix` 1.x has no `getrusage`. Hub memory on macOS is read with a bounded `ps -o rss= -p <pid>`
  (current RSS, which is what `cataly stats` should show); Linux uses `VmRSS` as designed.
- Secret masking uses a hand-written matcher for the built-in shapes; `regex` (part B) is used only for the
  owner's own patterns.
- `lane`/`thread` event fields exist in the store and the wire type but are always empty in this slice.

# Part B

## HTTP and Discord posting

- `relay::http` is written on hyper, hyper-util, http-body-util, tokio-rustls (ring) and webpki-roots
  instead of reqwest: the measured saving is above the 1 MiB threshold of `DESIGN.md` 10.2
  (`docs/measurements.md`). base64 for rpc v2 chunks is therefore the in-house strict decoder in
  `omp::rpc`, as 10.2 foresaw.
- Discord REST follows its own spec, DISC-OUT req. 4 and 7-9: 5 s connect, 10 s per attempt, reads and
  enforced-nonce creates retried on 5xx, 408 and network errors up to 4 attempts with backoff 1, 2, 4 s,
  all within 60 s. **Spec conflict recorded:** MSG-06 (shared messenger rules 9-10) says 15 s per request
  and 5 attempts. DISC-OUT is the narrower spec for Discord and agrees with `DESIGN.md` 9.4 ("Discord
  REST ... 10 s total"), so Discord uses it. `Policy` is per relay, so Telegram and Slack get the MSG-06
  values when they arrive. Writes that are not idempotent (`Kind::Once`) are never repeated after an
  uncertain outcome (MSG-06 rule 11).
- A 429 asking for at most 60 s is waited out and retried for every kind of request, within the same
  4-attempt, 60 s budget; a longer wait, or a wait the budget cannot hold, ends the call as rate-limited.
  For a job post that is recorded as a failed delivery naming the delay (rejected-with-delay, MSG-06
  rule 11) and is not repeated automatically. This cannot duplicate a post: a 429 means Discord did not
  process the request, and every create carries `enforce_nonce` with a nonce derived from the event, the
  destination and the part index, so any repeat inside Discord's dedupe window returns the existing
  message. Fixed in this pass: once an attempt may have taken effect (time-out, 5xx, 408, a dropped
  connection), a later 429, 4xx or refused connection in the same call no longer reports "rejected";
  the call ends uncertain (fail closed, MSG-05), and uncertain deliveries are never retried.
- One Discord post (all parts and their retries) has a 30 s delivery deadline; running out is uncertain.
- Outgoing text checks (unfilled `{field}`, owner placeholder words, owner banned characters) run before
  any request, so a refused post never reaches Discord.

## Owner jobs

- `regex` is built with `std` and `perf` only. Without the Unicode tables a pattern using a Unicode class
  (`\w`, `\d` in Unicode mode, `\p{..}`) fails to compile and the config check names the pattern; ASCII
  classes (`[0-9]`, `(?-u:\w)`) work. This keeps the Unicode tables out of the binary (not measured
  separately); secret shapes are ASCII.
- OJ req. 40 makes the config invalid when the hub's own config file or secrets directory lies inside the
  backed-up tree and is not ignored. Whether a path is ignored depends on the tree's `.gitignore`, which
  changes without a config reload, so backup asks `git check-ignore` at every tick and refuses that tick
  (one warning per reason) instead of judging it once when the config is read.
- The quota gate's owner lift is hub-wide until the latest reset of the current causes. A lift for a
  single launch needs session starts and comes with milestone 2.
- Followed PRs carry no lane or thread links yet; lanes arrive in a later milestone.
- Usage limits: OMP 19.1.1's verified `omp usage --json` shape has no reset time. The parser accepts the
  candidate fields `resetsAt`, `resetAt`, `window.resetsAt`, `window.resetAt`, `amount.resetsAt` (seconds
  or milliseconds or ISO time) and the window length from `window.durationMs`, `windowMs`, `durationMs`
  or the label (`5h`, `7d`). Without a reset time the gate releases at fetch time plus the window length,
  else after 5 h, so missing data never holds the gate for ever. Re-check when OMP documents the field.
- The few `#[allow(clippy::cast_*)]` attributes (pressure sampler, quota percent, usage instants, host
  headroom) are on float-to-integer and integer-to-float conversions, which have no lossless standard
  conversion; part A used the same form. No other lint is allowed; long functions were split.

## OMP probe

- OMP will not start in rpc-ui mode without a model configured. The hub's probe uses the owner's OMP
  configuration; when it has no model, the probe fails with OMP's message and health warns. The contract
  test gives OMP an isolated `PI_CODING_AGENT_DIR` whose `models.yml` names one loopback model on a port
  nothing serves; the probe sends no prompt, so no model is ever called.

## Tests

- `tests/support/fake-gh` and `fake-omp` are shell scripts answering from fixture files next to them;
  `tests/support/discord.rs` is a loopback fake Discord REST server on hyper that records requests,
  deduplicates creates by enforced nonce and can answer a create with a scripted status after creating
  it. `tests/owner.rs` covers acceptance checks 7, 8, 9, 10 (health) and 12 (old OMP) through the real
  hub with dry ticks; `tests/discord.rs` covers check 11 and the retry with the same nonce.
- The stall restart: under systemd the detector stops the watchdog pings and leaves the restart to
  systemd; without a watchdog (launchd, by hand) it ends the process with status 70 and launchd's
  `KeepAlive` restarts it. Both decisions are unit tests in `sys::stall`; a CI run under real systemd and
  launchd is not part of this slice.
- `tests/contract.rs` probes the installed stock OMP only when `CATALY_CONTRACT_OMP=1`
  (`CATALY_CONTRACT_OMP_PATH` picks the executable). CI runs it nightly against the latest OMP release,
  downloaded from its GitHub release and checked against `SHA256SUMS.txt`.

## Imports

- `import/m1/rpc_frames.rs`, `rpc_client.rs` and `fs_helpers.rs` are absorbed and deleted.
- Left for milestone 2: from `omp_probe.rs` the session launch and the identity read and resume check
  (the version read is absorbed); from `discord_rest.rs` bot identity and intent check, history, replies
  and their receipts, edit, clear, interaction callbacks, the seen reaction, DM channels and the
  unknown-message rule (the request policy, creates with nonces and the channel read are absorbed); from
  `discord_types.rs` message origin, interactions and the owner-only decision check (`Snowflake` is
  absorbed). `import/README.md` lists the same.

# Milestone 2 slice 2a: session host and lead

## API for slices 2b and 2c

- `hub.lead: Arc<host::lead::Lead<Headless>>`. `Lead::deliver(inbox::Message { sender, text, urgent,
  owner, identity, request_id }) -> Result<Receipt, String>`: stores the message in the lead's inbox
  (one SQLite commit), then delivers at once when the lead can read. `Receipt.state` is `waiting`,
  `sent` (OMP admitted, entry not yet confirmed), `delivered` (entry found in the session file),
  `in_doubt` (never resend), `failed` or `duplicate` (once-only identity seen). `Err` is a refusal;
  nothing was stored. Same `request_id` + same text returns the stored receipt; different text is
  refused. `Lead::receipt(request_id)` reads it again.
- 2b (owner messages): `owner: true, urgent: true`, `request_id` from the Discord message ID. Owner
  messages still go to a quota-blocked lead. 2c (events): `owner: false`, `urgent` per event kind,
  `identity` for once-only notices; quiet notes wait for the next wake and go out in one batch.
- Dialogs: `Lead::answer(dialog_id, &Value)` with one answer field (`value`, `confirmed`,
  `cancelled`, `answers`, `data` for forms); pending dialogs are in `Lead::view().session.dialogs`.
- Hub events added: `lead.started`, `lead.refused`, `session.stopped`, `session.fenced`,
  `session.spend_offer`, `session.fallback`, `session.fallback_recovered`, `session.quota_blocked`,
  `delivery.in_doubt`, `delivery.failed` (subject `lead`).

## Choices and deviations

- Delivery confirmation (PATCH-01 rule 8) for headless sessions: every batch starts with the marker
  `[cataly:<id>]`; it is delivered when a user `message` entry with the marker is in the session file
  (last 8 MiB, 2 s). The marker line also keeps text from starting with `/`. Contract test proves OMP
  writes the entry.
- In doubt: written but not answered (transport fault, session faulted) or no entry within 30 s with
  the same process alive. Resend once only when the process that held the batch is gone and the new
  process bound the same session ID (check 4); otherwise in doubt or failed. The lead's mailbox is not
  fenced; in-doubt rows are skipped and the lead is told once (COORD-13); fencing a Discord room is 2b.
- The session state sits behind a short `std::sync::Mutex` per session (never held across an await)
  shared by the event task and the one in-flight operation; reads go through a watch channel.
- Overlay (`<state>/omp-overlay.yml`, JSON is YAML) is written once per hub run. It pins the five
  saved-reset keys of OMP 19.1.1 (`codexResets.autoRedeem5h`/`autoRedeemWeekly`,
  `claudeResets.autoRedeem5h`/`autoRedeemWeekly`/`autoRedeemFable`) to `no`; see "Spend fixes"
  below. `goal.continuationModes` is `["interactive", "rpc"]`. `[fallback]` changes need a hub restart.
- `-e <bridge>` and the role instructions text come with 2c; `--append-system-prompt` takes `[lead]
  instructions` when set. `set_event_filter` is not sent yet (all events are read).
- The child environment is the runner's pass-through list plus listed credentials, `PI_*` profile
  overrides, `CATALY_SOCKET`, `CATALY_LAUNCH`. **Deviation from HOST req. 21 / DESIGN 4.2, decided:**
  the list adds USER, LOGNAME, LC_ALL and TZ to PATH, HOME, TMPDIR and LANG, and they stay. None is a
  secret (the purpose of HOST-06 is to keep secrets out), and the tools the agent runs through OMP read
  them: git and ssh take the user name from USER/LOGNAME, an owner who sets LC_ALL (for example
  `C.UTF-8` for stable sorting) or TZ expects `date`, logs and sorting in the agent's shell to match
  his own. One pass-through list for job commands and OMP children keeps one implementation
  (`sys::command`) for one concern.
- Goals (HOST-19), the transcript page (HOST req. 79) and view-token checks on calls are not built in
  this slice; the view token is in the snapshot.
- Memory admission applies only the memory part of the pressure rule (disk and session count are
  not start checks; the cap is). Usage-limit errors are recognised from `prompt_result.error` code or
  message text (OMP documents no code for it).
- Store schema v2 (additive): `sessions`, `inbox`, `deliveries`, `receipts`, `once_ids`.

## Gap closing after slice 2a

- **Lead restart.** When the lead's OMP exits by itself (not after `lead stop` or hub shutdown), the
  hub restarts it by the supervision rule of DESIGN 2.2: 1 s, doubling to 60 s; a restart that would be
  the sixth within 10 minutes is not made, the lead is held (`phase: held`, `session.fenced`, urgent)
  until `cataly lead start`, which also clears the count. Each restart resumes exactly the session that
  exited (same ID and transcript, checked like a resume) after `omp session lease` shows it free; a held
  or unknown lease holds the lead and alerts the owner (HOST req. 86-88). Every exit is a
  `session.stopped` event (HOST req. 70) whose text names the next step; only the first exit of a series
  is urgent. The lead hears it once: the resume note has the once-only identity of the session. A failed
  restart counts as a restart and is retried on the same schedule without an alert of its own; `lead
  stop` while a restart is planned cancels it.
- **Wake path, one commit.** `Mailbox::deliver` stores an arriving message and, when the lead can take
  it now (live, idle or working, no batch waiting for its entry, and the message may go), records the
  batch that carries it in the same transaction (`HostWrite::AdmitSend`) before the prompt is written;
  otherwise the message is stored and the inbox flushed as before. The fresh readback HOST req. 50
  requires stays (HOST req. 55 counts it in the budget), but runs while the commit waits for its fsync:
  `Control::refresh` overlaps it, and `Delivery.fresh` lets the session use that readback unless an event
  made it stale meanwhile. The flush path records its batch the same way.
- **Model fallback events.** OMP 19.1.1 sends `model_changed` without a model and reports a fallback
  with `retry_fallback_applied`, which its advisors also emit for their own model and which can repeat
  for one move. The session now derives model events from readbacks: one `ModelChanged` per model a
  readback shows, one `Fallback` when the model in use becomes the one OMP reported (a repeated report of
  the same move adds nothing; an advisor's report that moves nothing stays pending), one
  `FallbackRecovered` when the requested model is back (before, the restore was never seen with stock
  OMP). Measured against stock OMP: one `retry_fallback_applied` per move in a two-turn run; the
  contract test asserts exactly one `session.fallback`.
- **Flaky `stale_socket_replaced_live_one_refused`.** Root cause in the test, not the hub: it closed a
  listener and immediately expected the socket to refuse, but unit tests share one process, and another
  test thread that spawns a child at that moment holds a copy of every descriptor between fork and exec.
  The listening socket then still accepted for about 0.3 ms (observed: test-binary children with the
  copied descriptor table at the failure, refusal 0.27-0.44 ms later). The stale socket file now comes
  from a child process (the test binary running an ignored helper) that bound it and exited. The hub's
  rule (replace only when connecting is refused) is unchanged; a crashed hub's socket has no such copy.
- `tests/support/fake-omp` has an rpc-ui mode (fixture `rpc`): ready frame, every command answered,
  `get_state` naming the session it created or resumed. `tests/lead.rs` uses it for the restart test.

# Milestone 2 slice 2b: Discord intake, replies and questions

## Shape

- `relay::Relay` (DESIGN 5.1) is the per-platform trait: `parts` (outgoing checks, tables, split),
  `nonce`, `post` (one part, the first a reply), `seen`, `typing`. `relay::pipeline::Pipeline<R, T>` is
  the shared part: envelope, spill over 64 KiB to `<state>/files/relay/`, hand-off to the lead through
  `Lead::deliver` (`sender: owner`, `owner: true`, `urgent: true`, request ID `discord:<room>:<message>`),
  seen mark in parallel, owed reply, finish in one store transaction, hold and tell the owner, answers
  from the session file into the outbox and posted in parts. Telegram and Slack add a `Relay` and an
  intake; nothing after normalisation changes.
- `relay::discord::{gateway, intake, questions, types}`; REST calls stay on `relay::discord::Rest` over
  `relay::http` (one client). The Gateway's TLS uses `relay::http::connect` (rustls with ring, webpki
  roots), so `tokio-tungstenite` runs with only its `handshake` feature; `futures-util` (`sink`) is the
  stream and sink traits tokio-tungstenite already depends on (no added weight).
- Store schema v3 (additive): `bindings`, `positions`, `relay_flags`, `arrival_queue`, `handoffs`,
  `read_marks`, `owed_replies`, `outbox_parts`, `open_questions`. Rooms and messages are text so other
  relays fit; `seq`/`position` order a room (Discord: the snowflake).
- The intake task idles (state `off`) until `[discord] owner` and `channels` are set. Fatal conditions
  (bad token, not a bot, owner equals bot, no Message Content intent, binding to another bot or owner,
  a listed channel that cannot be read, close codes 4004 and 4010-4014) stop intake with an urgent
  `relay.stopped` event and wait for a config change or `cataly discord reset`; they never end the
  supervised task (that would count toward the hub's restart limit). Network failures retry after 30 s.
- Config `[discord]`: `owner`, `channels` (IDs, at most 256, duplicates dropped), `follow_threads`
  (default true), `home` (question channel; default the only listed channel), `remote_approvals`
  (default false, A18), `form_default_after` (default `3m`, 1 s to 1 h, A17), `profile` (default true:
  upload the bundled avatar `assets/avatar.jpg`, made for Cataly, when the bot has none). `cataly setup`
  manages `--discord-owner` and `--discord-channels` (comma list) besides the token file.
- CLI: `cataly discord status` (state, catch-up, queue, held rooms, owed replies, read positions),
  `cataly discord release <room>` (an in-doubt message counts as handled and is not resent),
  `cataly discord reset`. Events: `relay.held`, `relay.stopped` (urgent).

## Choices

- The Gateway URL: `wss://gateway.discord.gg`; with the loopback `api_base` test override it comes from
  `GET /gateway` and must be loopback `ws://`. Resume URLs must be `wss` under `discord.gg` (loopback in
  test mode); anything else identifies again (DISC-IN req. 19, DISC-OUT req. 10).
- Hand-off digest ignores the CDN `ex`/`is`/`hm` parameters; a changed message with the same ID holds
  the room (DISC-IN req. 51). A hand-off is recorded (`handoffs`, with the owed reply) before
  `Lead::deliver`; `Lead::deliver` is idempotent by request ID, so a crash between the two delivers once.
- In doubt: the mailbox already resends once when the process that held the batch is gone (check 4);
  a receipt `in_doubt` or `failed` holds the room (DESIGN 9.3), posts one reply in the room naming
  `cataly discord release <room>`, and emits `relay.held`. Other rooms continue.
- Read mark on durable admission (C9), recorded in `read_marks`; a failure is logged once per run.
- Answers (A16): the final assistant message (stop reason `stop`, no tool call, visible text) between
  the request's user entry (found by its tag `[discord <room>/<message>]`) and the next user entry. A
  later request in the same batch takes the answer; a request that met another input before an answer is
  abandoned (its owed reply stays open). The tag in a user entry is itself the hand-off confirmation, so
  an answer may be posted before the receipt is read again. "Turn over" is the turn-end/settle event or
  an idle snapshot. Answers older than 30 minutes without one are abandoned. The answer passes the
  owner's outgoing text checks; a refusal abandons it with a log line.
- Owed replies settle only by read mark plus confirmed linked first part (DISC-OUT req. 33, C8). The
  60 s reminder and the stop-gate continuation are 2c (bridge and unanswered job).
- Typing (C4): renewed every 8 s while a lead turn runs, in the room of the newest unanswered request.
- Questions: `ask` dialogs (buttons for one question unless multi-select), HTML forms read by
  `relay::discord::form` (buttons for one single-choice field), `select`, `confirm`; tool approvals only
  with `remote_approvals`. An `ask` with several questions and a form with several editable fields are
  posted as one card listing each question or field (name, kind, required, choices with their exact
  values, default) and answered by a reply holding a JSON map (DISC-OUT req. 63, 70, 72): unknown or
  hidden names are refused with a correction, omitted ones keep their default (an `ask` question without
  a recommended choice must be answered), required fields must end up non-empty; plain messages never
  answer such a card. The card is registered before its buttons become visible, so a press cannot find
  it missing. Buttons are refused for non-owner presses with an ephemeral reply; the owner's press is
  acknowledged (deferred update, or update for approvals) before the answer counts. The recommended
  choice is submitted explicitly after `form_default_after` (C11, no timed-out flag). Typed answers: a
  number, a unique label, or free text (`customInput`) for `ask`.
- Debug builds only (`cfg(debug_assertions)`; the release profile does not enable it):
  `CATALY_FAULT=relay_after_arrival` aborts the hub after the arrival commit and before the hand-off
  (crash test of check 4, which needs the real binary, so `cfg(test)` cannot carry it). The release
  binary holds no trace of it (`strings` finds no `relay_after_arrival`).
- Hub events of the lead and the relays go through one queue and are recorded one at a time in the
  order they were emitted (`hub::hub_events`); before, each event was its own task, so two events could
  commit in either order.

## Deviations

- Form fields carry no limits yet (`number` min/max, text lengths are not read), so the hub does not
  check DISC-OUT req. 72's ranges and lengths; everything else in req. 72 is checked.
- MSG-11 display name: Discord bot accounts always have a user name, so only the avatar is set
  (justified: nothing is empty to fill).
- DISC-IN req. 17-18, 22 are kept; DISC-IN req. 61's 8 s REST deadline is DISC-OUT's 10 s (the shared
  policy of part B). Catch-up 429/5xx retries use 2 s doubling to 30 s across rounds.
- Not built in this slice: channel facts and earlier-conversation lookup (DISC-IN-17/18, not in scope),
  thread-session routing (milestone 3), proof of source (DISC-IN-22, not in scope), Discord threads and
  board posts.

## Lead fixes found by this slice (host::lead, owner-approved)

- A batch that met the lead between states (a turn just ending, the readback not idle yet) went back
  to pending and only the 5-minute sweep retried it. The lead's event task now follows the session's
  snapshot (`Control::states`, a watch receiver; snapshots change only on events and readbacks, so this
  never polls) and flushes the inbox as soon as a readback shows the lead idle. A 5 s tick remains only
  as a backstop while a waking message is pending (`Mailbox::waking_pending`). Test:
  `tests/discord_relay.rs` `busy_lead_answers_in_order` asserts the second answer follows the first
  within 4 s (its turn takes 2 s; the backstop would add up to 5 s).
- `sys::lock` test: the re-acquire after dropping the lock retries for up to 2 s; another test thread
  forking at that moment holds a copy of the lock's descriptor until its child's exec (the same cause as
  the socket test above).

## Acceptance fixes (2026-11-25)

- **Test directories leaked (7,020 `cataly-unit-*`, 584 MB; contract sandboxes 3.7 GB).** Two causes.
  `sys::fs::tests::scratch` returned a bare path nobody removed; it now returns a `Scratch` guard that
  removes the directory on drop (also on a failing test, since unwinding runs drops), and the helpers
  built on it (`store::tests::open_temp`, `jobs::tests::ctx_with`, `host::testing::Fake`, the inbox
  tests) hold the guard as long as their store or session file lives. Second, every `TestHub` whose
  config names no OMP found the user's installed `omp` on PATH: the hub probed it with the sandbox as
  HOME, so stock OMP unpacked its 145 MB native module into `<sandbox>/home/.omp` and fetched model
  catalogues from the network; when the hub was killed the probe outlived it and wrote after the
  sandbox was removed. `TestHub` now strips every PATH directory holding an `omp` (contract tests name
  stock OMP by full path), stops a running hub with TERM before KILL so it closes its children, and the
  contract `Sandbox` and its `Rpc` children are removed and killed on drop. After three full runs and
  all contract tests: zero new entries in `/tmp`.
- **`discord_requests_against_stock_omp` never finished.** The test waited for `lead status` to show
  `working` while the slow turn ran. Evidence from the live sandbox at the timeout: the lead idle, 41
  owed replies settled, the slow answer posted, the follow-up never sent. The session status is
  `unknown` during a turn (an `agent_start` makes the last readback stale and nothing reads back until
  the turn ends; HOST req. 36 ranks "no fresh readback" above working), so `working` was never shown.
  The product follows the spec; the test now waits until the fake model has the slow request.
- **`lead_restarts_on_its_session_after_omp_exits` failed under load** (reproduced 10 of 24 runs with
  four copies in parallel): it read the event stream as soon as `lead status` showed the third
  process live and found one `lead.started ... resumed` instead of two. The phase turns live before
  the start event is recorded, and events were recorded by one task each. The event-driven flush of
  this slice did not cause it; it adds store work at the same moment, which likely widens the window. Fixes:
  hub events are recorded in emission order through one queue, and the test waits (bounded, on the
  condition) until the third start's record exists. 32 parallel runs pass.
- **Forms and multi-question `ask` dialogs stayed local.** DISC-OUT-15 is in milestone 2's scope and
  req. 59-61 list the reasons to refuse a form; "several fields" is not one of them. Both now become
  one card answered by a JSON map (see Questions above).

## Tests

- Unit: store relay rows (queue, positions, settling, binding), transcript replies, Gateway URL and
  close-code rules plus a fake WebSocket server (op 7 resumes, op 9 false identifies after 1-5 s, a
  missed ACK resumes, 4004 stops, 6 s identify gap), the owner gate and normalisation, presses,
  question conversion and typed answers, base64.
- `tests/support/discord.rs` is now a fake Discord REST and Gateway; `tests/support/fake-omp` runs turns
  (`turns`, `slow`, `noentry`, `ask` fixtures); `tests/support/model.rs` calls the `ask` tool on `ASK`.
- `tests/discord_relay.rs`: checks 1, 2, 3, 4 (crash after arrival; process gone resends once; process
  alive holds the room after 30 s), 5 (op 7 with catch-up, missed ACK, op 9, 4004, bad resume URL) and 7
  (refusal, owner press, local answer race, recommended after the quiet time).
- `tests/contract_discord.rs` (`CATALY_CONTRACT_OMP=1`): checks 1, 2 and 7 with stock OMP, the wake
  latency, RSS and catch-up measurements.

# Milestone 2 slice 2c: bridge, lead records, digest and unanswered watch, guidance, forms

## Decisions closing the slice (FinishM2c)

- **Unexpected model (DESIGN 6.1).** The session raises `ModelUnexpected` once per model when a
  turn is over (idle readback) on a model that is neither the expected one nor the fallback OMP
  reported. The expected model is the start model, or the one the hub asked for with `set_model`.
  Judging only at the turn's end avoids a false alarm when OMP's `retry_fallback_applied` arrives
  after the readback that first shows the new model (observed in the unit fake). The lead emits
  `session.model_unexpected`, urgent, and the lead hears it. Unit test
  `an_unreported_model_is_reported_once`; the stock-OMP fallback contract case asserts none is raised
  for a reported fallback.
- **Held rooms reach the lead (DESIGN 9.3, COORD-13).** `relay.held` is routed to the lead once, as
  a quiet note (it rides with the next wake): the owner is told in the room, only he can release it,
  and a session that just left a hand-off unconfirmed gets no extra prompt for it. Why the in-doubt
  test broke when the hold was routed: the lead's batch written while the fake session confirmed
  nothing (the urgent in-doubt notice, which the quiet note joins) must end, confirmed or in doubt
  after 30 s, before the next batch goes; the test waited only 30 s for the released message's
  answer after the release. It now waits 70 s (the 30 s confirmation window plus margin, as the hold
  wait already does) and asserts that the lead's inbox holds the held-room note exactly once.
- **Digest and unanswered settings stay under `[discord]`, not `[jobs.<name>]` (deviation from
  DESIGN 6.1).** `away`, `others`, `others_alert` and `others_drop` are per-relay facts: `others` must
  be a subset of that relay's watched channels and is validated against them, the digest fires on the
  relay's own owner message, not on a schedule, and the unanswered watch reads that relay's rooms. A
  `[jobs.*]` instance would need a relay reference and a trigger kind the scheduler does not have;
  Telegram and Slack (milestone 4) carry the same four keys in their own sections.
- **Bridge tool activation.** OMP 19.1.1 offers a tool an extension registers after load either as a
  top-level tool or, with its default `tools.commands`, as a command mounted in `bash` and listed in
  the system prompt; the contract case saw the second. Both are accepted.

## OMP surfaces checked against its documentation (19.1.1)

- **No flush before hello on a resumed session (EXT-35, rule 7).** The extension API has no flush;
  `extensions.md` says `appendEntry` "follows normal session persistence, so it does not force an
  empty session onto disk". The bridge therefore sends the hello with the ID OMP reports at
  `session_start`. For the headless lead the identity is still proven: the hub resumes only after
  `omp session lease` shows the session free, and `get_state` must report the same ID and file
  (HOST req. 27, slice 2a).
- **Background-job completion facts (EXT-16) are not built.** EXT-16 is not in milestone 2's scope
  (plan, milestone 2 bridge list); it serves lane evidence. OMP documents what it needs:
  `ctx.getAsyncJobSnapshot()` (read-only async-job snapshot) and the `tool_execution_*` events.

## Lint fixes

- Four functions over 100 lines were split (`cli::execute`: `bridge`, `job_hold`; `hub::serve`:
  `open`, `stream_cwd`; `post_card`: `mark_question`, `post_parts`, `first_message` returns the ID;
  `store::lead::write`: `question_ask`, `question_settle`); panicking JSON indexing in the question
  panel became map inserts; the rest were mechanical (`map_or_default`, `write!`).

## Contract fix: the start recap is a note, not unsettled work (FixContractDiscord)

- **All three `contract_discord` cases timed out in `settled()`, before the first owner message.**
  Evidence at the timeout (`lead inbox --json`, now printed by the test's panic): one pending entry,
  sender `cataly`, preview `[recap] Cataly's records now (read them before acting on re…`. The lead
  was live and idle, the bridge connected, nothing sent or in doubt. It was neither suspect: no room
  was held (`discord status` showed `held: 0`) and no model event was raised (the session read back
  `fake/primary`, the requested model). It is the recap `hub::bridge` queues when the lead's bridge
  says hello on a fresh session (EVR req. 48).
- **The note is right; the settle check was wrong.** The recap is a quiet note by design: EVR req. 48
  adds it to the lead's context, it does not start a turn, and a note "waits for the next one"
  (glossary, Delivery). The test read `lead status` `waiting`, which counted every pending entry,
  wake or note, so a lead with only a note waiting looked unsettled forever, and the view gave no way
  to tell the two apart. `lead status` now reports `waiting` (pending entries that
  start a turn: urgent or the owner's) and `notes` (pending quiet notes) apart, and every inbox entry
  carries `wakes`; `cataly lead inbox` shows a pending note as `note`. Every test that waits for
  `waiting == 0` now means "nothing that should wake the lead is pending", which is what it asked.
- `discord_requests_against_stock_omp` now asserts the start recap is the one note waiting
  (`wakes: false`, `notes: 1`) and that the inbox is empty after the first wake: the recap went out
  with it.

## Tests

- `tests/contract_session.rs` `bridge_in_stock_omp`: stock OMP with an isolated agent directory and
  the bridge linked by `cataly bridge install`. A probe extension loads the installed bridge through
  a counting proxy of the extension API: an ordinary session registers exactly `session_start` and
  `session_switch`, no tool and no entry. Through the real link, a socket listener at `CATALY_SOCKET`
  sees no connection, no request carries the guidance or the tool, and no session holds the marker.
  `bridge_accepted_lead_in_stock_omp`: the hub's lead connects, the request carries the guidance and
  the tool, and the session file holds the marker. `tests/support/model.rs` now records request
  bodies.
- `tests/contract_discord.rs` `rss_with_discord_lead_and_owner_jobs`: the RSS measurement below.

# Milestone 3 slice 3a: lane lifecycle, reports, oversight

## Choices

- **Oversight is a supervised task.** The lanes' sweep (reminders, stall alert, stuck verdict,
  ceilings, thread posts) runs inside `Lanes::run`, spawned under the hub's `Supervisor` next to the
  lead and Discord intake, woken by a `Notify` on every lane event and by its own next-due time. It
  is not a `[jobs.*]` instance: its timings are per lane and come from lane records, not a schedule,
  and a crash restarts it with the other supervised tasks.
- **Progress marks are computed from reports.** A lane's last progress is the newest report of the
  current run (`store::lanes` reports) or the run's start, whichever is later; no separate mark is
  stored. Turn activity (`watch.active_ms`) only feeds the quiet rule for thread posts. The decision
  itself is the pure `oversight::decide` with unit tests.
- **A lane does not wait for its bridge hello at start.** The start is confirmed by the kickoff entry
  in the session file and the goal readback (HOST-19), as for the lead. Until the hub accepts the
  lane's bridge hello, the bridge holds no shell rules from the hub and blocks every shell call (fail
  closed, EXT req. 32); a disconnect blocks them again.
- **The lane's handle is saved before OMP is spawned** (found by the stock-OMP contract case). The
  bridge says hello during OMP's start; the record held the new handle only in memory until the start
  returned, so the hello found no open lane, the bridge went inert for the session and the lane
  never got its tool. The fake-OMP tests missed it because the test connects later.
- **OMP asks before the lane tool runs.** Under the lane's approval mode, stock OMP 19.1.1 shows
  "Allow tool: cataly_lane"; the lane is blocked until the lead answers it (the contract case
  approves it with `lane answer`). Exempting Cataly's own tool from approval is left to the owner.
- **Shared GitHub helpers.** `done` checks of kind `pr_merged` call `gh` the way the owner jobs do:
  `jobs::owner::gh_api`, `GH_ENV` and `gh_answer` are shared, with the bound kept at the caller.
- **A closed run's pending notes are retired.** On close, rows still pending in the lane's inbox (the
  quiet claim notice) are ended (`HostWrite::Retire`, state `failed`) and counted in the close
  receipt; a reopen starts a new run whose inbox holds only what the reopen sends. Written or
  in-doubt rows are not touched: a close already waits until nothing that wakes the lane is unsent.
- **Lane start time is in `cataly stats`** (`lane_start_latency`: spawn to kickoff confirmed).

## Deviations

- The board, slots and planner are slice 3b; `lanes::record::kickoff` takes an empty step list.

## Lint fixes

- Split: `relay::pipeline::hand_off` (lead delivery to `to_lead`), `lanes::call` (`ledger`,
  `reasons`), `lanes::start` (`start_spec`), `life::reopen` (`new_run`), `config::lanes::parse`
  (`parse_oversight`), `hub::bridge::serve` (`pump`), the fake Discord's `answer`; `hub::sessions`
  returns the `Sessions` type. The rest were mechanical (`map_or`, `map_or_else`, a collapsed `if`,
  `Self::build`, a synchronous `read`).

## Tests

- `tests/lanes.rs`: checks 1-10 on the fake OMP; check 4 also reopens the closed lane and asserts
  the claim notice was retired and never reaches the new run. `oversight_cpu_with_eight_lanes`
  (ignored) measures the hub's CPU.
- `tests/contract_lanes.rs` (`CATALY_CONTRACT_OMP=1`): stock OMP lanes with the bridge linked. One
  open-report-claim-close cycle, the report made by the lane's model through the bridge's
  `cataly_lane` tool; and check 10 (three lanes, two with active goals resumed, one paused stays
  paused, the recap). `tests/support/model.rs` answers `CALL <tool> <json>` with that tool call.
- `omp::transcript::tests::spend_read_cost` (ignored) measures spend reading.
- `tests/support/fake-omp`: the background-turn poller tested its parent with `kill -0`, which a
  killed, unreaped parent (a zombie) passes; the poller then held stdout open, the hub never saw the
  exit and `lead_restarts_on_its_session_after_omp_exits` timed out. It now treats a zombie as gone.

# Milestone 3 slice 3b: coordination

## Shape

- **Slots** (`lanes::slots`, `cli::slot`): a fair in-hub queue per slot name, priority then strict
  arrival. A holder keeps its slot until both its client connection and its command exited. Grants
  and queues live in memory only; after a hub start nothing is granted for 5 s so running clients
  can re-assert their holds. `cataly slot <name> [--lane] [--priority 1-9] [--timeout] -- <cmd>`
  runs the command in the caller's terminal and ends with its exit status. `lane.slots` and
  `lane.admission` are the views (WS-37).
- **Queued launches** (`lanes::queue`): an open naming predecessors (`after`) is validated and its
  brief assembled at once, the key reserved; the lead hears once per change when it becomes ready or
  is held again. The hub never starts it; `lane.start` or the owner does.
- **Items and planner** (`lanes::plan`): items are a queue of later work; the planner pass
  reconciles items with their lanes, audits, starts the next items under the lead, the quota gate,
  admission and the lane maximum, asks open questions through the lead, runs the owner-notes step
  and stores the run (168 kept). A pass that found nothing says nothing to the lead.
- **Owner notes** (`jobs::notes`): the profile file of 2c is now the owner notes: off until the
  owner creates them, five fixed sections, dated one-line entries; the hub checks (lost decision,
  future date, token-shaped string), snapshots and reverts (a backup is saved first).
- **Review and merge gate** (`lanes::review`): a review runs a fresh print-mode OMP (or the
  configured reviewer command) pinned to the PR head and stores its last verdict line; the merge
  refuses without an approving verdict at the current head, with no checks, or with a check neither
  passed nor skipped, then squash-merges pinned to that head; `dry_run` stops before merging. It uses
  the shared `gh` helper of the owner jobs. Event commands are argv templates, at most four running,
  never through a shell.
- **Published files and learned rules** (`lanes::publish`): a file is registered with its hash,
  size, title, visibility and link under a configured root; a change adds a version. Nothing is
  uploaded. Rules are one-line lessons, global or per repository, put before new briefs.
- **Board posts** (`lanes::board`): one message per `[coord] board` destination, rebuilt when lanes
  change and edited at most once a minute, only when the body (without its update time) changed. The
  create is stored as pending first, sent with an enforced nonce (the relay's retries are safe); an
  outcome still uncertain after the retries marks the destination in doubt, and it is neither
  recreated nor edited, also across restarts, until the lead names the message it found
  (`lane.board-found`), which wakes the next pass to edit that message.

## Records in the `coord` table

Migration 6 adds one table, `coord (kind, id, version, at_ms, doc)`: one JSON document per kind and
id, written only by the hub with compare-and-swap on `version` (`lanes::docs`). The store knows no
fields. Kinds and their ids: `queued` (lane key), `item` (item id), `planner` (`state`), `plan_run`
(start time, zero-padded; 168 kept), `verdict` (`owner/repo#N@head`), `file` (the resolved path),
`rule` (rule id), `board` (destination, e.g. `discord:<channel>`). Slot grants are not stored.

## The `approval` extension surface

Stock OMP 19.1.1 asked "Allow tool: cataly_lane" before every lane tool call (slice 3a). The bridge
now registers each Cataly tool with the extension API's `approval: { tier: "write", policy: "allow" }`
(OMP `approval-mode.md`: tool tier and tool policy). The hub checks every operation these tools
request, so OMP need not ask; a user `tools.approval.<tool>: deny` still wins, and a restricted
folder still prompts. `tests/contract_lanes.rs` no longer answers the dialog and asserts none opened.

## Choices and deviations

- The planner is a supervised task beside oversight (`Lanes::plan_loop`), not a `[jobs.*]`
  instance: it needs the lanes and runs once at start, then at last run plus the interval.
- Items carry their own done checks, since every lane needs one.
- The lead's notes save refuses a failing check instead of masking secrets in place.
- While a slot command runs, a terminal Ctrl-C is not passed on again (the command already got it);
  TERM and HUP are.
- A queued launch must name `thread`, not `thread_from`.
- The planner checks the quota gate before "no lead".
- `lane.board-found` wakes the oversight task, so the found message is edited at once rather than at
  the next sweep.
- **A quota tick waits for the first OMP probe.** The second full run failed once at the baseline
  tick of `owner.rs`'s quota test (15 repeats of that binary passed; the event list was not
  captured). The control socket serves before the OMP watcher's first pass, so a `cataly job run`
  of a quota job right after start can read with no resolved executable and report `tick.failed`;
  that race fits the failure. The tick now waits for the first probe report
  (`Omp::first_probe`), bounded by its own deadline.

## Tests

- `tests/coord.rs`: checks 1-6 through the real hub on the fake OMP and the fake `gh`; check 6 also
  through the fake Discord: two destinations, one whose create is stored but answered 500 on all
  four attempts (one message, thanks to the nonce). It asserts one create and one edit after at least
  59 s holding both changes of that minute, no create or edit in the doubtful channel, nothing
  recreated after a hub restart, and that `board-found` makes the next pass edit the found message.
  A first version scripted a single 500 and saw the relay's nonce retry settle the create; that is
  correct, so the test now keeps the outcome uncertain through every attempt.
  `planner_pass_with_100_items_and_rss` (ignored) is the measurement.
- `tests/support/discord.rs`: `script_in` scripts create statuses for one channel.
- `lanes::board::tests` keeps the pure step cases: the minute rule on exact times, a refused create
  that may be tried again, and a pending mark that cannot be stored stopping before the create.
- Closing runs: `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean; the full
  `cargo test --no-fail-fast` passed twice in a row after the last change outside the ignored
  measurement (198 unit tests passed, 3 ignored; every integration binary); each contract binary
  passed alone in release with `CATALY_CONTRACT_OMP=1` against stock OMP 19.1.1 (`contract` 1,
  `contract_session` 5, `contract_discord` 3, `contract_lanes` 2, the lane tool running with no
  dialog); `bun test extension/` 6 passed.

# Milestone 4: Telegram and Slack

## Shape

- **Shared intake** (`relay::intake`): one supervised start-serve-stop loop and one round that drains
  the durable arrival queue through the pipeline, for all three relays; Discord moved onto it. Each
  relay runs in its own task with its own queue, so a stalled one never delays another (MSG req. 2;
  `a_hung_relay_never_delays_another` hangs each fake in turn).
- **Telegram** (`relay::telegram`): long polling, at most 100 new or edited messages and a 25 s
  wait; offset, batch time and replay guard in the store (migration 7, one JSON document per relay)
  after each update; the identity and webhook checks, 409, 401 and 429 rules, the owner's DM or the
  configured group and topic, General as topic 1. Telegram intake refuses to start on Windows
  (owner decision 4).
- **Slack** (`relay::slack`): DMs, group DMs, mentions and untagged replies in threads the account
  posted in; each thread is its own room, `<channel>:<root ts>`; answers go into the request's
  thread; `cataly slack upload` with the `files.slack.com` check before any byte; the `eyes` seen
  mark, skipped for the run when the app lacks `reactions:write`.
- **Channel facts and earlier conversation** (`relay::context`, DISC-IN-17, DISC-IN-18): for every
  relay, off by default under `[context]`.
- **Destinations**: jobs and the board post (`relay::boards`) reach Telegram and Slack; a board
  create on either is one message with no nonce, so an uncertain create leaves the destination in
  doubt, as the board rules want.
- `cataly telegram|slack status|release|reset`; `cataly setup` has the Telegram and Slack settings
  (tokens stay in owner-only files), and the lead's guidance explains the Slack app or member choice.

## The Slack transport: Socket Mode for both roles

Both roles, the app's bot user and a member through the app's user token, use one installed app in
Socket Mode (`apps.connections.open` with the `xapp-` token, then the WebSocket; every envelope is
acknowledged within 3 s, after the event was admitted or dropped). Slack's Events API documentation
delivers an app's event subscriptions, including events on behalf of users, over Socket Mode or a
request URL, with no bot-only limit; Socket Mode needs no public endpoint, which a request URL would
(SLACK req. 34). After start and every reconnect, remembered threads and DMs are read from their
positions, so an envelope missed while disconnected is caught up from history.

## The bounded inbox (carried fix from slice 3b)

- Recurring summaries (planner runs, lane recaps, notes refreshes, digests) carry a replacing
  identity, `latest:<kind>:<subject>`: a new one retires the undelivered earlier one, so with no lead
  they leave one entry, not one per pass.
- **Cap:** 200 undelivered non-owner messages per target (`store::host::INBOX_CAP`). **Overflow
  rule:** admitting one more retires the oldest undelivered non-owner message (state `failed`, its
  receipt kept 7 days). Owner messages are never retired and do not count toward the cap.
  `inbox_is_bounded_and_keeps_owner_messages` covers both.
- The long planner run (`docs/measurements.md`, milestone 4) shows the inbox was not the cause of
  slice 3b's late RSS rise: the rise is still there with the summaries replacing one another. The
  cap stays, as a bound on a store table that had none.

## Choices and deviations

- **A Telegram edit reaches the lead, with no seen mark.** Reviewed against the spec: MSG-01 makes
  "edit" an event kind, TELE req. 18 polls edited messages, TELE req. 26 keeps the original message
  ID for an edit, and MSG req. 6 gives the seen mark to new messages only. The spec is not silent, so
  it wins over consistency with Discord (whose edits are never delivered, DISC-IN req. 15). The edit
  goes as its own request labelled "this edits the owner's earlier message N", and its answer
  replies to that message. `tests/relays.rs` now asserts the label and that no seen mark is added.
- An uncertain answer part on Telegram or Slack ends that answer instead of being sent again:
  neither platform enforces a nonce (MSG req. 8).
- Plain channel messages in Slack without a mention, outside a remembered thread, are dropped.
- Not done in this milestone: the Slack bot name and avatar are not set (MSG req. 14), and Slack
  destinations are not tagged with the workspace.
- **A thread reply is not judged before its thread is recorded.** `slack_threads_allowlist_acks_and_uploads`
  failed in both of the first two full runs at a wait for an answer in the thread (the panic named
  the fake's 30 s wait; its message was not kept); it passed alone and in a third full run. The
  account records a thread only after the post's response, so a reply the owner sends as soon as
  the post is visible can be read before the record exists and dropped as untagged, and nothing
  reads it again until a reconnect; that race fits the failure [inference, not reproduced
  deterministically]. Every post now holds a shared lock (`slack::POSTING`) from the request until
  its thread is recorded (`post_and_record`, used by answers and destinations alike); an untagged
  reply in an unrecorded thread waits for the posts in flight, at most 1 s of its 2.5 s admission
  budget, then looks again. 24 parallel runs of the test passed after the change.

## Lint fixes

- Optional JSON body fields are inserted through `as_object_mut` instead of indexing.
- `slack::upload` lost its byte transfer to `slack::transfer`; `cli::setup::sources` lost the
  per-setting values to `value_of` and the relay settings to `relay_value`.
- `room`/`root` bindings became `room`/`thread_ts`; the two `typing` methods that cannot type
  return a ready future; the fakes' helpers consume their values.

## Tests

- `tests/relays.rs`: checks 1-5 through the real hub, the fake Telegram and the fake Slack and the
  fake OMP; check 5 also covers the edit decision above.
- `tests/contract_relays.rs` (new): check 5 against stock OMP on the loopback fake model with
  Discord, Telegram and Slack all connected to their fakes: a Telegram and a Slack request each reach
  the lead and are answered in place with a seen mark, then 20 more per relay for the
  measurements.
- Closing runs: `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean; the full
  `cargo test --no-fail-fast` passed twice in a row after the last code change (280 tests passed, 5
  ignored, over the unit tests and every integration binary); each contract binary passed alone in
  release with `CATALY_CONTRACT_OMP=1` against stock OMP 19.1.1 (`contract` 1, `contract_session` 5,
  `contract_discord` 3, `contract_lanes` 2, `contract_relays` 1); `bun test extension/` 6 passed.

# After the first CI run: macOS lint, a flaky lane test, decision 15, fallback setup

## macOS lint (CI run 45562347823)

- Clippy on macOS failed on dead Linux-only helpers. The cgroup and `/proc/meminfo` parsers,
  `V1_UNLIMITED`, `headroom_percent`, `limit_value` and `headroom()` itself are now
  `cfg(target_os = "linux")`; their unit test runs on Linux, the load-average test everywhere.
  `jobs::owner::pressure` reads container headroom through a Linux-only function; elsewhere it is
  `NoLimit` (a `const fn`), and `Headroom::Percent` exists on Linux only, so no wrapped `Result` is
  left on macOS.
- Checked locally with `cargo clippy --target x86_64-apple-darwin --all-targets -- -D warnings`:
  the target's std was added to the user's rustup toolchain (nothing system-wide); the C parts of
  the bundled SQLite and `ring` cannot be compiled here without a macOS SDK, so the check ran with a
  stand-in C compiler that writes empty objects (`CC_x86_64_apple_darwin`, in `/tmp`, not in this
  repository). Clippy links nothing, so the Rust lint result is the real one; linking and tests on
  macOS remain CI's.

## The flaky held-lane test

`pause_resume_and_a_held_lane_that_turns` failed on the CI runner waiting for the second
"held lane active" alert. Reproduced here: 2 of 20 parallel runs failed under 16 busy loops.
**Root cause, a real ordering bug:** the lane watch decides that an active stretch is over from the
session's snapshot at a turn-end event, but every turn-end event marks the snapshot stale (status
unknown) and only the readback it queues can show the session idle, and that readback emits no event.
When the readback landed after the watch had handled the last turn-end event (a slow machine), the
stretch was never closed and the next turn of the held lane was never reported. The watch now waits,
bounded by the rpc response time, for the first fresh snapshot after the event
(`lanes::life::idle_after_readback`). After the fix: 60 of 60 runs passed (three rounds of 20 in
parallel under 16 busy loops).

## Decision 15: the early wake

- `host::inbox::Mailbox::wake_early`: an owner request (with a request ID, no once-only identity)
  that goes to the lead alone, when the store would admit it (`admits_at_once`, a read), is written
  to OMP while the `AdmitSend` commit runs. Its delivery ID is `inbox::wake_id(target, request ID)`,
  so its marker is known without a record. Anything else (lanes, hub notes, batches carrying other
  rows, CLI sends without a request ID) commits first as before.
- `relay::pipeline`: the relay's hand-off record commits alongside the lead's wake. A message not
  read live in this hub run goes through `Lead::written_earlier` first: its marker in the current or
  previous lead transcript means delivered (the hand-off is recorded and finished, so the answer is
  still posted); not found with a lead of this hub run means not delivered; no lead yet defers the
  room (`Step::Deferred`, tried next round; later messages of the room wait behind it); a
  transcript that cannot be searched holds the room in doubt. A wake written whose record then fails
  to commit is in doubt, never resent.
- Crash points (debug builds, `CATALY_FAULT`): `wake_before_commit` (nothing written or committed),
  `wake_after_write` (written; the commit is held back so it cannot land), `wake_after_commit`.
  The points now share one helper, `sys::fault`, with the earlier `relay_after_arrival`,
  `lane_after_close_intent` and the unit tests' `after_event_append`.
- Not covered by the early path: a message queued while no lead runs now waits in the relay queue
  rather than in the lead's inbox until the lead is up; an owner stop leaves new live messages in
  the inbox as before.

## Fallback setup

`cataly setup --fallback-on <any|usage-limit|except-usage-limit> --fallback-chain
<provider/id,...>` writes `[fallback] on` and `[fallback.chains] default` with the same snapshot,
preview, validation, `--apply --expect` and source report as the other settings; the lead's
guidance tells the owner how. The overlay carrying it is written once per hub run, so a change
applies from the next hub start, which the guidance says. Only the `default` chain is managed by
setup; other roles stay hand-edited.

## Tests

- `tests/discord_relay.rs` `crash_at_each_wake_point_delivers_once`: a crash at each point, then a
  restart: one user entry in the lead's transcript, the room not held, the queue finished, at most
  one answer (the answer is checked when nothing was written before the crash).
- `tests/hub.rs` `setup_sets_the_fallback_chain`: refusals name the flag, the preview writes
  nothing, apply against the preview's hash, `config check` accepts the result.
- Closing runs: `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean on Linux,
  and the same clippy with `--target x86_64-apple-darwin` (as above); the full `cargo test
  --no-fail-fast` passed twice in a row after the last code change (283 passed, 5 ignored, over the
  unit tests and every integration binary); each contract binary passed alone in release with
  `CATALY_CONTRACT_OMP=1` against stock OMP 19.1.1 (`contract` 1, `contract_session` 5,
  `contract_discord` 3, `contract_lanes` 2, `contract_relays` 1); `bun test` in `extension/` 6
  passed.

# Faster Discord wake (FastWake, 2026-11-25)

## What was measured

The Discord path of a live owner message was stamped in a release build at frame read, parse, owner
gate, arrival commit, round pickup, route, the lead's `deliver`, the readback, the prompt write, the
hub's read of OMP's `agent_start` and the relay round's sight of it (a throwaway build; the stamps
are not in the tree). Figures are in `docs/measurements.md`. Two findings:

- The arrival commit (8.2 ms, one `synchronous=FULL` fsync) was the only large hub cost before the
  prompt write; everything else the hub does before the write took about 1.1 ms, plus the 2-2.3 ms
  `get_state` readback HOST req. 50 requires (it carries the spend fence, so it stays).
- The end-to-end figure itself was late by 8.6-9.4 ms at p50 and 35 ms at p95: the relay round sees
  the lead's events only between rounds, and it was still waiting in `deliver` for the inbox commit
  when `agent_start` came. The "about 20 ms of intake" noted under decision 15 was mostly this.

## Changes

- `relay::pipeline::Pipeline::arrive` (Discord only, `Relay::LIVE_ONCE`): a live owner message the
  store would queue now (`Reads::arrives_at_once`: the room has a position below it and is not held,
  nothing of the message is stored, the queue has room) is put in the writer's queue
  (`Store::relay_queued`) and handed to the round at once; the round drains it next to the stored
  queue. The writer applies requests in queue order, so the hand-off record and the lead's records
  commit with or after the arrival, never before it. The Gateway task still waits for the commit
  before reading the next frame. The read times of live messages moved from both intakes into the
  pipeline (one implementation).
- Recovery is the decision-15 one: a crash before the arrival commits leaves the room's position
  below the message; the catch-up read at the next connect finds it as a message not read live, and
  the lead's transcript decides before any send. A new Gateway session never replays a message, so
  it cannot come back live.
- Telegram and Slack keep the arrival commit first: both redeliver what was not acknowledged as live
  messages in the next hub run, which would skip the transcript lookup and could deliver twice.
- The wake latency is now taken up to the moment the lead reads `agent_start` (`Lead::turn_started`,
  stamped before its followers see the event) instead of when the relay round gets to it.
- Fewer serial reads: `Mailbox::deliver_as` decides with one read (unconfirmed batches, open rows,
  early admission) instead of three; `Pipeline::hand_off` takes the hand-off record its caller just
  read instead of reading it again.
- Found while checking every early write: `cataly lead send --request-id` took the early wake with no
  transcript lookup, so a retry with the same ID after a crash between the write and the commit
  could reach the lead twice. The early wake is now opt-in (`Lead::deliver_live`, used only by the
  relay pipeline, whose callers either read the message live or searched the transcript);
  `Lead::deliver` commits first again.

## Lane messages and lead messages without a request ID: not changed, and why

- Without a request ID nothing derives the delivery marker before a record exists: the batch ID is
  random and lives only in the record. An early write whose commit does not land leaves nothing to
  look up, and if OMP did not keep the entry (it died with the hub) the message is lost; committed
  first, the flush resends it once into the resumed session. Hub notes, lane notices and
  `cataly lead send` without an ID stay commit-first.
- Lead-to-lane messages with an ID arrive as fresh calls from the lead, so a retry after a hub crash
  looks like a first send. Telling them apart would need a search of the lane's transcript before
  every send, which costs more than the 8 ms commit as the file grows, plus the in-doubt fence of EVR
  req. 62 for a write whose record failed. Owner messages routed to a lane's thread could reuse the
  relay rule, but would need a second transcript lookup over lane sessions (closed and restarted
  ones included) for a path with no wake target; not done (one implementation per concern).

## Tests

- `tests/discord_relay.rs` `crash_at_each_wake_point_delivers_once` gains `arrival_after_write`: the
  arrival is held back, the prompt is written, the hub dies; the store holds no queue item for the
  message, and after the restart the catch-up read finds it, the transcript shows it written, and it
  is delivered once, answered at most once, nothing held. The three earlier points now run with the
  arrival overlapping the hand-off. `relay_after_arrival` keeps its order (arrival committed, nothing
  handed): debug builds take the commit-first arrival when that point is named.
- `tests/lead.rs` `request_id_sends_commit_before_the_write`: with `wake_after_write` named, a CLI send
  with a request ID does not trip the point (the write comes after the commit), and after a crash
  the retry with the same ID returns the same inbox row; the transcript holds the text once. It
  failed against the previous behaviour.
- Closing runs: `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean; `cargo test
  --no-fail-fast` passed twice in a row after the last code change (284 passed, 5 ignored); `cargo
  build --release`; each contract binary passed alone in release with `CATALY_CONTRACT_OMP=1`
  against stock OMP 19.1.1 (`contract` 1, `contract_session` 5, `contract_discord` 3,
  `contract_lanes` 2, `contract_relays` 1).

# macOS test failures (FixMacos, 2026-11-25)

CI runs 45571895529 and 45582137796 (darwin-arm64) failed three `tests/discord_relay.rs` tests.
`cargo test` stops at the first failing binary, so `hub`, `jobs`, `lanes`, `lead`, `owner` and
`relays` had never run on macOS; run 45583627584 used `--no-fail-fast` and ran them all.

## The state directory behind a symlink (crash and unconfirmed hand-off tests)

- Evidence: after a hub restart the lead ran a new session file in the same sandbox, and the hub
  log said `lead starts fresh: the transcript of lead session … did not verify for an exact
  resume`. The unconfirmed hand-off then met "another session" and held the room
  ("the lead may or may not have it") instead of resending once.
- Cause: on macOS `/tmp` and `/var/folders` are symlinks into `/private`. The hub gave OMP
  `--session-dir` under `$XDG_STATE_HOME` as spelled, recorded the transcript path OMP reported,
  and the exact-resume check (`omp::transcript::exact`, HOST req. 9-10) refuses a recorded path
  that differs from its real location. Lanes resume through the same check.
- Fix (DISC-IN's OS note: state paths are resolved to their real location first):
  `Paths::real_state` resolves the state directory once at hub start, and the hub derives every
  state path from it. The resume check still refuses symlinks below the state directory.
- Tests: `tests/lead.rs` `lead_resumes_through_a_symlinked_state_directory` reaches the state
  directory through a symlink on any system; on main it fails on Linux (a fresh session after the
  restart) and passes with the fix. Unit: `sys::paths` `real_state_resolves_a_symlinked_state_directory`.

## The busy lead's second answer 5 s late (`busy_lead_answers_in_order`)

- Evidence: 7.05 s in both runs, the 2 s turn plus the lead's 5 s backstop tick.
- Cause, in the fake OMP: it answered `get_state` from a busy flag it removed (a forked `rm`)
  only after printing the turn's end events. When the readback those events trigger arrives
  first, it shows the lead still streaming, no later event brings another readback, and the
  waiting message goes at the tick. Stock OMP clears `isStreaming` before it emits `agent_end`
  (pi-agent-core `Agent`, the `agent_end` case) and answers `get_state` synchronously. Reproduced
  on Linux by delaying the `rm` 0.2 s: 7.04 s, with a readback answered busy after `agent_end`.
- Fix: the fake clears the flag before the end events (unless follow-ups wait to run next); with
  the same 0.2 s delay the gap is 2.2 s.

## The crash test read the transcript too early (`crash_at_each_wake_point_delivers_once`)

- Evidence (once the resume worked): `wake_after_commit: written before the crash`, 0 entries.
- Cause: OMP answers `prompt` before the prompt runs (rpc-mode: "Don't await - events will
  stream"), so its user entry lands after the answer; the hub died at the fault point right
  after the answer and the test read the file before the fake appended it.
- Fix: the test waits until the lead's OMP process is gone, when its file is final (the rule the
  hub's own confirmation uses).

## After the release merge

- `tests/install.rs` (four tests, darwin only): `held: …/home/.local/bin/cataly exists and is not
  Cataly's link`. `Layout::of_running` finds the program folder through the binary's real path
  (`/private/tmp/…`) while the links were written with the default spelling (`/tmp/…`). It now
  keeps the default layout when the binary's folder is the default folder under another spelling.
- `tests/owner.rs` `home_backup_setup_and_restore` (both systems on CI): the test's bare
  repository took git's default branch (`master` on the runners, `main` where it was written), so
  the clone of the pushed `main` was empty. The bare repository now starts on `main`.
- `TestHub` prints the last 200 lines of the hub log when a test fails.
- After the voice merge both CI jobs failed to build `audiopus_sys` 0.2.2: its bundled libopus
  declares `cmake_minimum_required` below 3.5, which the runners' CMake 4 refuses.
  `.cargo/config.toml` sets `CMAKE_POLICY_VERSION_MINIMUM=3.5` under `[env]`, so local builds and
  CI build it the same way (CMake 3.x ignores the variable).

# Spend fixes (FixSpend, 2026-11-25)

Release-review findings Safety 2 / Design 1, Design 8, Safety 7 and Safety 8. Everything about OMP
here was read from the installed omp 19.1.1 (its binary's strings and `omp read omp://…`) or run
against it in an isolated profile.

## Saved-reset keys (Safety 2, Design 1)

- The earlier note above was wrong: 19.1.1 has no `codexResets.autoRedeem`. Its schema has five
  enum keys (`unset | yes | no`, default `unset`, no environment binding): `codexResets.autoRedeem5h`,
  `codexResets.autoRedeemWeekly`, `claudeResets.autoRedeem5h`, `claudeResets.autoRedeemWeekly`,
  `claudeResets.autoRedeemFable`. Its changelog says they replace each provider's `autoRedeem`.
- Measured in an isolated profile whose global config said `yes` for Claude 5h and Fable: with the
  old overlay (`codexResets.autoRedeem: no`) OMP migrated that key into the two Codex keys, but the
  Claude keys stayed `yes` (layer `global`). With the new overlay all five read `no` (layer
  `overlay`). Overlays outrank project and global config; only environment bindings and runtime
  overrides rank higher, and these keys have neither.
- `launch::overlay` writes the five keys from one list (`NO_REDEEM`); the old key is gone. The unit
  test checks all five and the old key's absence. `contract_session::lead_end_to_end_and_resume`
  starts stock OMP with the hub's own overlay, a global config saying `yes` to every key, and a test
  extension that reads each key through OMP's public settings registry (`lookup(id).get` and
  `.provenance` on `pi.pi.settings`); all five must read `["no", "overlay"]`.

## Review gate (Design 8)

- The built-in reviewer now runs `omp --print --no-session --approval-mode always-ask
  --config=<lanes' overlay> --tools=read,grep,glob --max-cost=<usd> [--model=…] <prompt>`. In print
  mode nobody can approve, so every call outside the read-only set is refused. The cost cap is the
  lanes' `ceiling_cost`, else 2 USD.
- With no shell, the reviewer cannot run `gh`; the hub fetches the PR (title, description, first 100
  changed files with their patches) into a private file under `files/review/`, rechecks that the head
  did not move, names the file in the prompt (marked as the author's text, not instructions) and
  deletes it after the run.
- Found on the way: 19.1.1 refuses `--tools` with an unknown name (`ls`) or a tool the session lacks
  (`find` needs a native judge model): `Unknown tool in --tools: ls`. The look-only set
  (`LOOK_ONLY`, OPC-46) was `read,grep,find,ls`, so a look-only lane could not start. It is now
  `read,grep,glob`, shared by look-only lanes and the reviewer (checked by hand with `omp --print`).
- Test: `review::tests::the_reviewer_runs_fenced` (exact argv) and
  `review_text_lists_files_and_says_what_is_left_out`.

## Tern and extra usage (Safety 7)

- 19.1.1 keeps the usage-limit state (`getUsageLimitState`, the source of `get_state.usageLimit`) on
  its session object; the extension context (`ctx`) built in the binary has no field or method for
  it, and `omp://extensions.md` documents none. So the bridge cannot forward it, and a tern session
  cannot be fenced before its next request the way a headless one is.
- Instead a tern lane is refused at open, reopen and restart while the account-limits read (`omp usage
  --json`, the existing cached reader) lists a paid limit for the lane model's provider, or cannot be
  read. 19.1.1's Claude usage provider lists `anthropic:extra` (`scope.windowId: extra`, unit `usd`)
  exactly while extra usage is enabled on the account; any `usd` limit (a prepaid extra balance)
  counts too. Not verified against a live account with extra usage; the shape is read from the binary.
  The headless runtime is unchanged and still fences on `get_state.usageLimit.extraUsage`.
- Tests: `usage::tests::paid_extra_usage_is_told_apart_per_provider`;
  `tests/tern.rs::a_tern_lane_is_refused_while_its_provider_reports_extra_usage` (unreadable limits,
  then a provider with an `extra` window: both refused, nothing recorded or started).

## Home backup visibility (Safety 8)

- With `include_cataly`, every push first reads the push URL as configured and as git expands it
  (`insteadOf`). Each must be a GitHub repository that `gh api repos/<owner>/<name>` reports
  `private: true`, or a path on this machine (a local bare repository leaves nothing on GitHub).
  Public, missing, unreadable or another host refuses the push with a `backup.push_failed` alert
  (`refused: … is public …`), once per reason; the commit stays and the next tick checks again.
- Setup and the job share one reader of `gh`'s answer (`backup::private`) and one repository-name
  check (`backup::valid_repo`).
- Tests: `backup::tests::remote_places_and_visibility_answers`; `tests/owner.rs`
  `home_backup_setup_and_restore` keeps the GitHub URL, redirects it to a local bare repository with
  `url.<bare>.insteadOf`, makes the fake `gh` report the repository public (push refused, nothing
  in the bare repository), then private again (pushed).

# Session authority (FinishAuthority, 2026-11-25)

Review findings Safety 1, 3, 4, 5, 6 and Design 3, 12, 13.

## Control callers get their session's role (Safety 1, Design 3)

- The hub places each control caller by its peer PID and that process's ancestors
  (`sys::ancestry`: `/proc` on Linux, one bounded `ps` on macOS) against the OMP processes it
  spawned and the peers of accepted bridges (`Links::adopt`/`place`). Authority is `Owner`,
  `Lead`, `Lane(key)` or `Unplaced`; the method table says which methods need `Owner`. A caller
  that carries a launch token but cannot be placed fails closed. `whoami` reports the placement;
  `setup --apply`, `install`, `update`, `rollback`, `uninstall` and `service` ask it first.
- A bridge peer that is one of the hub's own ancestors (whatever started the hub: the owner's
  shell, a service manager, a test) is not adopted: every owner command descends from it too.
  The fake bridges in `tests/lanes.rs` and `tests/tern.rs` connect from the test process, which
  started the hub; adopting it placed every owner CLI call of those tests in the session's role.

## Rule 0 everywhere, one tokeniser (Safety 3, 6, Design 12)

- The lead's `bridge.shell` is judged by rule 0 (`lanes::shell::rule_zero`); onboarding's own
  tokeniser is gone. Rule 0 finds `omp usage … reset` in any argument order, `omp config set`
  of any `*Resets*` key, variable indirection, and refuses a command word it cannot resolve.

## The socket directory is checked before connecting (Safety 5)

- The CLI, the slot and the bridge connect only when the socket's directory is the user's own,
  mode 0700 or stricter, and not a symlink (`extension/cataly.ts` `privateDir`, checked inside
  `unixConnect`). The CLI honours `CATALY_SOCKET`, so a lane's `cataly status` reaches its hub.

## Subagents and `eval` (Safety 4)

- OMP 19.1.1 binds the extension again for every subagent session (`ExtensionAgentIdentity`,
  `kind: "sub"`, `parentId`). A subagent of an accepted session now connects with `agent: "sub"`
  and its parent: the spawner's session when the spawner's bridge runs in the same realm, else
  (inside a hub launch, e.g. a worker thread) the parent file in its session header. The hub
  resolves a file to the root session ID (`hub::bridge::root_session`: the stamped session file
  or its artifact directory, below which OMP keeps nested subagent files) and gives the worker
  its parent's role for shell rules only: no guidance, no tool.
- The subagent's `tool_call` veto is registered before it connects, so a hub that is down, slow
  or refuses the parent blocks its `bash` and `task` calls (fail closed, A9). A subagent of a
  session the hub never accepted stays inert.
- `eval` (Python/JS cells that can start any process) is not judged by shell rules. The overlay
  sets `tools.approval.eval: deny`; OMP's approval docs say a user `deny` always denies, and
  the contract case confirms it holds in a lane's subagent of stock OMP 19.1.1, so no `--tools`
  list is needed.

## The bridge is typed against OMP (Design 13)

- `extension/package.json` pins `@oh-my-pi/pi-coding-agent` 19.1.1 (dev dependency, type
  imports only, erased at load). Handlers take OMP's event types (`ToolCallEvent`,
  `BeforeAgentStartEvent`, `MessageEndEvent`, `ToolExecutionStart/EndEvent`, `SessionStop…`);
  model lookup uses the documented `ctx.models` (`current`, `resolve`) instead of
  `modelRegistry`. `bun run typecheck` in `extension/` is clean. `cataly.ts` is 599 lines.
- CI does not run `bun test` or the type check yet (GitHub Actions is unavailable); run both in
  `extension/` before a merge.

## Tests

- Bun, `extension/authority.test.ts` (shared fakes in `extension/fakes.ts`): a lane's subagent
  connects as `sub` with its parent and its shell calls are judged; a subagent whose hub gives
  no verdict runs no shell command, and one of an unaccepted session stays inert; the bridge
  connects only through a private socket directory (mode, owner, symlink, real socket).
  `cataly.test.ts`'s terminal test now resolves models through `ctx.models.resolve`. All four
  fail against the bridge of main 9d6b31d (run with its `cataly.ts` in place).
- Rust unit: `hub::bridge` `a_subagents_parent_resolves_to_its_root_session`; earlier on this
  branch `lanes::shell`, `cli::client`, `sys::paths`, `omp::launch`
  `overlay_denies_eval_in_every_session`, and `tests/lead.rs`
  `a_session_child_is_refused_owner_methods_and_a_plain_shell_is_the_owner`.
- Contract (`CATALY_CONTRACT_OMP=1`): `tests/contract_lanes.rs`
  `a_lane_subagent_is_held_by_shell_rules_and_eval_deny_in_stock_omp`: a stock-OMP lane spawns a
  subagent through `task` (the test approves every dialog, so only the rule under test can
  refuse); its `bash` gets the hub's "Blocked by shell rule 1", its `eval` gets OMP's
  "blocked by user policy", and the cell's output never appears.
- `tests/lanes.rs` `onboarding_consent_is_stored_and_writes_wait`: after onboarding ends, rule 0
  still refuses the lead's `cataly lead send` (it was allowed before rule 0 covered the lead).

# CI covers the voice build (CiVoice, 2026-11-25)

## What CI runs

- Each build job (linux-x64, darwin-arm64) keeps the default-feature steps and adds clippy and
  tests with `--workspace --features voice`. `--workspace` matters: without it Cargo builds the
  `cataly-voice` crate only as a dependency, so its own tests (`crates/voice/tests/transports.rs`)
  and unit tests were never linted or run. The voice release build's size is printed beside the
  default one; the idle-memory figure stays on the default build.
- Tests run with `--no-fail-fast`, and the voice and Bun test steps run even when an earlier test
  step failed, so one run shows every failure on a platform.
- The bridge tests run with `bun test` in `extension/`, Bun 1.4.0 from `oven-sh/setup-bun` pinned
  by commit.
- The nightly job (and a manual dispatch) runs every contract binary, `--test 'contract*'`, in
  release, instead of `contract` alone: `contract_tern` and the other stock-OMP suites had never run
  on CI.
- The job timeout is 90 minutes: two feature sets, each tested and built in release.

## First run (45598912381): two macOS failures

- `tern::a_tern_lead_resumes_in_tern_whatever_the_config_says`, both feature sets: "lead starts
  fresh: the transcript … did not verify for an exact resume". The test plays the bridge and
  reported the transcript under `TestHub::state()` as spelled (`/tmp/…`), while the hub hands OMP
  the real state directory (`/private/tmp/…` on macOS, since the FixMacos change) and the exact
  resume check refuses any other spelling. The test was wrong, not the hub: OMP reports the path
  under the directory it was given. The fixture now writes and reports the transcript below the
  canonical state directory.
- `cataly-voice` `check8_transports_agree` (first run of the crate's tests on CI): "continued once,
  after every call", 2 continuations instead of 1. **A real bug in the session:** after a tool
  answer it asked the backend to continue (`response.create`) as soon as no call was open, even
  while the backend's response was still running and could still name more calls. When the first
  call was answered before the later calls of the same response arrived, the backend got a
  continuation per gap. The session now continues only when every call is answered and the
  response has completed; calls answered while it runs are continued once, at its completion. The
  fake realtime server's new `call_gap_ms` spaces the calls and the completion, and `pass_outcomes`
  (checks 4 and 8, both transports) uses 300 ms: before the fix that run gave 4 continuations on
  Linux over both transports, after it 1.

## Second run: contract suites on CI (dispatch 45603874106) and main's admission default

- `contract_tern::tern_lead_in_stock_omp` failed on macOS against stock OMP 19.2.0: the test read
  the session's memory and CPU from `/proc`, which macOS lacks. Off Linux it now reads
  `ps -o rss=,time=` (`time` is `[h:]m:s.cc` there; Linux's `ps` gives whole seconds only, so Linux
  keeps `/proc`).
- Main at a0a20c9 made the admission rule without a pressure job the pressure job's defaults,
  50 GB free disk among them (LANE-08; the product default stays). The runners have about 12 GB
  free, so every lane start in `tests/coord.rs` was held ("free disk 12 GB below 50 GB").
  `TestHub::write_config`, the one place every test config passes through, now adds a disabled
  pressure job `test-admission` with memory, disk and session floors 0, unless the test names its
  own pressure job; no test depends on the host's free disk or memory. The default refusal is
  checked once, on a faked reading of 12 GB: `pressure::tests::the_default_rule_refuses_a_small_disk`.
- `relays::telegram_and_slack_requests_are_answered_in_place_with_seen_marks` failed on macOS:
  `slack_wake_latency` empty although the Slack request was answered. **A real race in the
  pipeline:** the wake time was set only after `deliver_live` returned (the Slack arrival commits
  first), and the lead's turn-start event, read meanwhile, found no wake and was dropped; no later
  turn start came, so the wake was never recorded. Setting the wake now settles it at once when the
  lead's turn already started at or after it (`Pipeline::settle_wake`, under the same lock as the
  event path, so it is recorded once).
- `contract_tern::tern_lane_held_while_working_in_stock_omp` timed out on Linux (OMP 19.2.0) waiting
  for the kickoff to show working under an active goal; it passed alone here against the same
  release. Its wait now names what it last saw. Three copies of the `contract_tern` binary at once
  here: two passed, one failed `tern_lead_in_stock_omp` (`lead stop` refused) and
  `tern_lane_lost_and_restored_in_stock_omp` ("goal not set: the bridge did not answer goal within
  10 s") in its first 25 s, while three stock OMPs started together: these are the load-dependent
  cases, not fixed here.
- Under load the stock-OMP tern cases fail at the lane's goal: the hub's `goal` command through
  the bridge (OMP `sessionModes.set`) gets no answer within the 10 s command bound
  (`host::tern` `COMMAND_WAIT`), the lane is held by the hub, and the kickoff is never sent
  (CI dispatch 45609104039, macOS; the same reason in the local three-copy run). Not fixed here:
  why OMP's mode change takes over 10 s while several TUIs start is still open.

## Scope added from the design review

- `release.yml` builds with `--features voice` on both targets (coordinator decision: releases
  ship with voice).
- The contract job runs every contract binary on pull requests, pushes and release calls against
  pinned stock OMP 19.1.1, and nightly (or on dispatch) against the latest release; Bun is set up
  in it. The Tern runtime cases use the test's Tern stand-in, so nothing is skipped.
- A `cargo deny check` job (EmbarkStudios/cargo-deny-action pinned by commit), and a bridge
  type-check step that runs `bun run typecheck` once `extension/package.json` defines it.

## Local runs

- A `CARGO_TARGET_DIR` shared with another checkout is unsafe for the hub's integration tests:
  they run `target/debug/cataly`, which a concurrent build of the other checkout can replace with a
  binary built without `voice`. Two local runs of `tests/voice_hub.rs` failed in every case that
  way and passed with the same code once nothing else built [inference from the pattern; the
  replaced binary was not captured].
- `tests/install.rs` builds its release variants once into `target/install-variants` and reuses
  them by version alone. After a change to the bridge, the cached V1 variant carries the old
  bridge, and `restart_holds_while_a_session_is_busy` fails with "the bridge … is not this
  version's" (seen here once); delete that folder after such a change. CI starts without it.

## Tern commands lost under load: the bridge dropped them (root cause)

- The open question above is answered: OMP's mode change was never slow. Reproduced with 16 busy
  shell loops on the 16-core host and three copies of `contract_tern` running the lane cases
  (three of six failed with "goal not set: the bridge did not answer goal within 10 s"). A trace in
  the bridge (timestamps of every socket read, line, command start and end, and an event-loop stall
  monitor; removed after the run) showed, in every failure, the `goal` command in the **same socket
  read** as the hello answer, and no command start for it at all. No event-loop stall, no "busy"
  refusal.
- Why: the hub wakes the start when it binds the hello (`Port::attach`), before it writes the hello
  answer, and the lane's goal follows a store write later; the bridge reads both lines when it is
  slow to read. `receive()` ran each command only when `this.tern` was set, and `connect()` sets it
  in the continuation of the hello request, one microtask after the read's line loop; so a command
  read with the hello answer was dropped without a reply, and the hub waited out its 10 s bound.
  In the cases that passed, the goal came 20-40 ms after the hello answer, in a read of its own.
- Fix: every command goes on the bridge's command chain, and the chain decides by `this.tern` when it
  runs, after `connect()` applied the hello answer (`extension/cataly.ts` `receive`). Bridge test
  "a command in the same read as the hello answer runs once the answer is applied" sends both in one
  read; it failed before the change (no reply) and passes after.
- The 10 s command bound stays: with the fix, under the same load with nine stock OMPs at once, all
  six lane goals came in the same read as the hello answer and OMP took 104-247 ms to enter goal
  mode, pause and resume 0-4 ms, with no "busy" refusal (saved modes were already restored) and no
  event-loop stall over 0.6 s except the test's own SIGSTOP. All nine cases (three copies of
  `tern_lane_held_while_working`, `tern_lane_lost_and_restored`, `tern_lead`) passed.
- `tern_lead_in_stock_omp` stopped the lead right after the tenth wake reached the model, while that
  turn could still run, and `lead stop` refuses a working lead; it now waits for the lead to settle
  first and prints the CLI's output on failure. Which of the two causes produced the earlier
  "`lead stop` refused" is not known [its output was not printed].

## Test processes outlived their tests

- Earlier runs left 22 Tern stand-in wrappers (`python3 -c 'import pty …'`, re-parented to init),
  each with a stock OMP from a `/tmp/cy-*` sandbox, about 2.9 GB. A Tern block outlives the hub by
  design (lanes keep running across a hub restart), and nothing ended the stand-in's blocks: a
  lane test that failed stopped its hub in `Drop` and deleted the sandbox under the running OMP;
  `nohup` made OMP ignore the hang-up of its terminal, and `tern close` killed only the wrapper.
  The fake OMP's TUI mode (`exec sleep 600`) leaked the same way for ten minutes per block.
- The stand-in's block wrapper (Python, in `fake-tern`) now runs in its own process group and
  records its program's PID (`block-<id>.omp`; the program leads its own session and group, as
  `pty.fork` makes it). The program gets the parent-death signal SIGKILL on Linux, so it dies with
  the wrapper. On TERM, INT or HUP the wrapper ends the program's group (TERM, KILL after 3 s) and
  exits; when the program exits it kills what is left of that group.
- A parent-death signal cannot tie the wrapper to its test: its parent is the stand-in script,
  which exits as soon as it printed the block ID. The wrapper therefore polls (every 0.5 s, on
  Linux and macOS) the test process that installed the stand-in, whose PID `TestHub::install_fake`
  writes to `<name>-data/owner`, and ends its block once that process is gone. So a block also
  survives a hub restart within its test, as a real Tern block does.
- A test killed with SIGKILL runs no `Drop` and its hub would run on. `TestHub::spawn_hub` starts
  the hub through `/bin/sh`, which leaves a poller (`kill -0` on the test process and the hub every
  second) and then execs the hub, so the hub keeps the PID tests read; the poller sends the hub
  TERM once the test process is gone and exits with the hub.
- `TestHub`'s `Drop` (normal end, panic, failed assertion) stops the hub as before, then sends TERM
  to every recorded block process group, KILL after 5 s; then it lists processes whose command
  line names the sandbox (`ps -A -o pid=,args=`), waits up to 2 s for them to go, kills those that
  stay and fails the test naming them (when the test is already failing, it prints them).
  `tests/tern.rs` checks both: `a_block_ends_when_its_owner_is_gone` and
  `a_process_left_in_the_sandbox_fails_the_test`.
- Shown here: `contract_tern` (stock OMP) SIGKILLed together with its cargo while five stand-in
  blocks ran (five wrappers, five TUI OMPs, one headless OMP, five hubs and their pollers): 3 s
  later no process naming those sandboxes and no hub of this checkout ran; the same for
  `tests/tern.rs` (two blocks, fake OMP). Every wrapper and program PID the two runs recorded (16)
  was gone. Both suites pass in full with the leak check on.

# Merge of fix/spend, fix/authority, ci/voice and wip/first-run (MergeAll, 2026-11-26)

## Conflicts

- `omp::launch::overlay`: fix/spend pins the five saved-reset keys from `NO_REDEEM`; fix/authority
  adds `tools.approval.eval: deny`. The overlay keeps both (both unit tests run unchanged).
- `tests/tern.rs`: fix/spend's extra-usage refusal test and ci/voice's `lead_transcript` comment.
- `extension/cataly.ts` merged without conflict: fix/authority's bridge (subagent veto, socket
  folder check, OMP types) with ci/voice's command chain behind the hello answer; 599 lines.
- Build notes and the clean-room log keep every branch's sections.

## Timing bounds under load

- `contract_session::bridge_in_stock_omp` (the bridge's import and factory inside stock OMP) and
  the Bun test "an ordinary session ... (check 11)" (the inert bridge's start-up) each asserted a
  5 ms bound on one measurement. Under host load (a parallel `cargo test`, other agents' builds)
  one run can be descheduled for longer than the whole cost while the code is unchanged: fix/spend
  saw `bridge_in_stock_omp` fail only under parallel load and pass alone.
- Both now measure seven times and assert the 5 ms bound on the fastest run, and print every
  figure. Load only adds time, so the fastest run is the closest to the code's own cost; a real
  regression slows every run and still fails. The contract case starts stock OMP seven times
  with the probe (each run also re-checks the inert registrations).
- Measured on this merge (`contract_session` alone, release, stock OMP 19.1.1, host busy with
  other agents): 5.84, 3.07, 1.62, 1.58, 1.72, 1.63, 1.34 ms; the first run alone would have
  failed the bound. The Bun test's runs: 0.003-0.037 ms.
- Checks: `cargo test --no-fail-fast` twice (370 passed, 5 ignored) and with `--workspace
  --features voice` twice (426 passed, 7 ignored); each contract binary alone in release with
  `CATALY_CONTRACT_OMP=1` (`contract` 1, `contract_session` 5, `contract_discord` 3,
  `contract_lanes` 3, `contract_relays` 1, `contract_tern` 5); `bun test` 13 passed;
  `bun run typecheck` clean; no process naming a `/tmp/cy-` sandbox left after any run.

## The owner's local day (`profile_check_and_revert`)

- `tests/coord.rs` `profile_check_and_revert` failed on every run between 00:00 and 09:00 KST: the
  check refused today's entries as `future date`. The test takes today from `date` (local), the
  hub from `jiff::Zoned::now()`, and jiff without its `tz-system` feature answers UTC for the
  system zone. COORD req. 52 counts snapshots by the local day, and the away digest prints local
  times through the same system zone, so both were in UTC on every machine.
- `jiff` now enables `tz-system` (`TZ`, else `/etc/localtime`). It brings no crate on Linux or
  macOS (`windows-link` on Windows only; `Cargo.lock` gains that edge, no package).

## CI and release

- `ci.yml` installs the bridge's locked dev dependencies (`bun install --frozen-lockfile`, OMP's
  types and TypeScript) and runs `bun test` and `bun run typecheck` in `extension/` on both
  targets; the "when defined" guard is gone now that fix/authority added the script.
- `release.yml` writes each target's third-party notices in the build job
  (`scripts/third-party-notices.sh` with the target triple, which the script needs for
  `cargo fetch --target`; `linux-x64` is not a triple) as `THIRD-PARTY-NOTICES-<target>.txt`, so
  the two targets' files do not collide when the publish job merges the artifacts. The publish job
  adds `LICENSE`; both are listed in `SHA256SUMS` and uploaded with the other assets. Neither name
  matches `cataly-<version>-<target>`, so `install.sh` and `cataly update` still find one binary.

# The lead's tool in its shell (FixLeadTool, 2026-11-26)

## What failed

On the owner's first run the lead could not record onboarding. OMP 19.1.1 offers an extension
tool as a command of its embedded shell (`tools.commands`, on by default; `tools/bash.md` "Tool
commands", `custom-tools.md`): a mounted tool is resolved after the shell's builtins and in-process
utilities and before `PATH`, and `runToolCli()` runs it in OMP's process, not as a subprocess. The
lead's tool was named `cataly`, the CLI's name, so `cataly --help` in its shell printed the tool's
op list, and `cataly onboarding.start; cataly requests` met the bridge's `bash` veto, where rule 0
cannot tell the tool command from the CLI and refused it. `eval` (`tool.cataly(...)`) is denied by
the overlay, by design. The lead told the owner it could record nothing.

## Fix

- The lead's tool is `cataly_lead` (`hub::tool::LEAD_TOOL`); lanes keep `cataly_lane`. Neither
  can be the CLI, so rule 0 keeps matching only the real `cataly` binary, and the CLI is no longer
  hidden behind the tool command in the lead's shell.
- The hub's shell veto passes a `bash` line that only runs the session's own tool as a tool
  command (`lanes::shell::tool_only`): every segment's command word is exactly that name, and no
  redirection, substitution or expansion appears outside single quotes (OMP's own condition for a
  tool-only line). A lane's report whose text matches a deny rule therefore passes; a path, a
  runner, an assignment or any other command on the line is judged as before. A subagent has no
  tool (the name would reach `PATH` there), so its lines are always judged.
- Rule 0's refusal for the lead names `cataly_lead`; the lead's guidance says every op is called
  through `cataly_lead`, directly or as `cataly_lead <op> --flag value` in bash, and that the
  `cataly` CLI runs only its read-only verbs there. Lane evidence counts a `bash` call of either
  tool command as organising, like the CLI.

## Tests

- Contract (`CATALY_CONTRACT_OMP=1`): `tests/contract_session.rs`
  `lead_tool_works_as_a_call_and_as_a_shell_command_in_stock_omp`. The fake model calls the tool as
  `bash` with the owner's line (`cataly_lead onboarding.start; cataly_lead requests`) and directly
  (`onboarding.start` answers "already runs", `requests`); `cataly lead send hi` in the lead's
  `bash` gets rule 0 naming `cataly_lead`; `eval` gets OMP's "blocked by user policy". With the
  tool name set to `cataly` and run against main 7d53196, the first step fails with the owner's
  rule-0 refusal. The fake model now gives each `CALL` its own call ID (`call_named_<n>`): the hub
  answers a call ID once, so a second call reusing one got the first call's answer.
- Rust unit `lanes::shell` `only_the_sessions_own_tool_command_passes_as_a_tool_line`;
  `tests/lanes.rs` `shell_rules_block_and_grants_are_one_attempt` (a lane report matching a deny
  rule passes as a tool line; with a second command or by path it does not) and
  `onboarding_consent_is_stored_and_writes_wait` (the lead's tool line passes, the CLI's refusal
  names `cataly_lead`).
