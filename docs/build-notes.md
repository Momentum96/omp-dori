# Build notes: milestone 1, part A

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
- Secret masking uses a hand-written matcher for built-in shapes; `regex` is not a dependency until owner
  patterns (backup job, part B) need it.
- `lane`/`thread` event fields exist in the store and the wire type but are always empty in this slice.
