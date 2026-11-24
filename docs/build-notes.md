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
- Overlay (`<state>/omp-overlay.yml`, JSON is YAML) is written once per hub run. OMP 19.1.1 has one
  saved-reset key, `codexResets.autoRedeem` (set `no`); the five keys in DESIGN 4.2 are not in its
  schema. `goal.continuationModes` is `["interactive", "rpc"]`. `[fallback]` changes need a hub restart.
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
