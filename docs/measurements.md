# Measurements

The figures behind the targets in `DESIGN.md` 2.3, by milestone.

# Milestone 1

## Part A

Machine: Linux 7.0.0 x86_64, 16 cores, local disk. Toolchain 1.100.0, `cargo build --release`
(`lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`, `panic = "unwind"`). Measured 2026-11-25 on a
temporary state directory and socket; the real user state was never touched.

| Measure | Result | Target (DESIGN 2.3) |
| --- | --- | --- |
| Hub idle resident memory, ten timed (cron) jobs configured, 10 s after start | 7.0 MiB (`VmRSS` 7112 kB) | ≤ 30 MiB total (no relays yet) |
| Hub start to control socket answering `status` | 57 ms first start (store created and migrated), 15-20 ms warm starts | ≤ 150 ms |
| `cataly status` round trip over 100 calls (process start, connect, request, reply, print) | p50 3.3 ms, p99 7.2 ms | p50 ≤ 20 ms |
| Release binary size (linux-x64 glibc, stripped) | 4,395,480 bytes (4.2 MiB) | none set |

Hub-side figures from `cataly stats` in the same run: control request handling p50 134 µs, p99 293 µs;
store commit (WAL, `synchronous=FULL`, one fsync) p50 8.3 ms on this disk.

## Commands used

```sh
cargo build --release
R=/tmp/cy-measure; rm -rf $R; mkdir -p $R/{home,config/cataly,state,run}; cd $R
for i in $(seq 1 10); do printf '[jobs.t%s]\nkind = "cron"\ncron = "%s 3 * * *"\nzone = "Europe/Berlin"\nargv = ["true"]\ncwd = "/"\n\n' $i $i; done > config/cataly/config.toml
chmod 600 config/cataly/config.toml
export HOME=$R/home XDG_CONFIG_HOME=$R/config XDG_STATE_HOME=$R/state XDG_RUNTIME_DIR=$R/run
B=<repo>/target/release/cataly
# start to socket answering, five starts (the first creates the store)
for n in 1 2 3 4 5; do s=$(date +%s%N); $B hub & P=$!; until $B status >/dev/null 2>&1; do :; done
  e=$(date +%s%N); echo "start_to_status_ms=$(( (e-s)/1000000 ))"; kill -TERM $P; wait $P; done
# idle memory
$B hub & P=$!; until $B status >/dev/null 2>&1; do :; done; sleep 10; grep VmRSS /proc/$P/status
# status round trip, 100 calls
for i in $(seq 100); do s=$(date +%s%N); $B status >/dev/null; e=$(date +%s%N); echo $(( (e-s)/1000 )); done \
  | sort -n | awk '{a[NR]=$1} END {print "status_p50_us="a[50]" p99_us="a[99]}'
$B stats; kill -TERM $P; wait $P
ls -l $B
```

Not measured in part A (see part B below): RSS with all owner jobs, CPU per idle hour, the
reqwest-versus-hyper comparison, incremental debug build time. Still open: macOS figures (CI prints
binary size and idle RSS on `macos-latest`), RSS with relays connected (the Gateway arrives in
milestone 2).

## Part B

Same machine and toolchain, measured 2026-11-25 with the final part-B release binary on temporary state
directories (commands below). The owner-jobs run
configures every owner kind against fakes: `github` and `pr` with the fake `gh`, `quota` with the fake
`omp`, `backup` on a local git tree, `health` with one file check, `items` with a one-line script,
`pressure`; all enabled and live, so each took its first tick at start and the fast ones (pressure 60 s,
health 2 min, github and pr 3 min) kept ticking during the window. Both runs also have the ten cron jobs
of part A and the fake `omp` as `[omp] path`. No relay is connected in milestone 1.

| Measure | Result | Target (DESIGN 2.3) |
| --- | --- | --- |
| Hub idle resident memory, ten cron jobs, 10 s after start | 9.1 MiB (`VmRSS` 9328 kB; 9416 kB after 10 min) | ≤ 30 MiB total |
| Hub idle resident memory, ten cron jobs and all seven owner jobs, 10 s after start | 9.9 MiB (`VmRSS` 10128 kB; 10304 kB after 10 min) | ≤ 30 MiB total |
| Added by all owner jobs | 0.8 MiB (800 kB at 10 s, 888 kB after 10 min) | < 5 MiB |
| Hub CPU time while idle (utime + stime of the hub process, children excluded), 600 s window | 0.09 s without owner jobs, 0.14 s with them: about 0.5 s and 0.8 s per hour | none set |
| Hub start to control socket answering `status`, owner-jobs config, warm | 16-19 ms | ≤ 150 ms |
| `cataly status` round trip over 100 calls | p50 2.7 ms, p99 7.0 ms | p50 ≤ 20 ms |
| Release binary size (linux-x64 glibc, stripped) | 7,955,736 bytes (7.6 MiB), up from 4.2 MiB in part A | none set |
| Incremental debug build of `cataly` after touching one module | 4.7 s | split trigger 30 s (`DESIGN.md` 10.1) |

The binary grew by 3.4 MiB in part B: TLS (rustls, ring's TLS use, webpki roots), hyper, regex and the
owner jobs. The part-A idle figure (7.0 MiB) had no OMP probe, no HTTP stack and no owner code paths.

### reqwest versus hyper (`DESIGN.md` 10.2)

Measured on this machine before `relay::http` was written,
each variant added to the part-A binary with the same release profile, and not repeated here:

| Variant | Binary added | Idle RSS added |
| --- | --- | --- |
| reqwest (`rustls-no-provider`, `json`, no defaults) with ring | +2.57 MB | about +1.8 MiB |
| hyper + hyper-util + http-body-util + tokio-rustls (ring) + webpki-roots | +1.23 MB | about +0.75 MiB |

hyper saves about 1.3 MB of binary and about 1 MiB of resident memory, over the 1 MiB threshold, so
`relay::http` is written on hyper and reqwest is not a dependency.

### Commands used (part B)

```sh
cargo build --release
# For mode in base, owner: sandbox under /tmp/cy-m-$mode with home, config, state, run; fake gh and
# omp taken from tests/support with fixture files; config = [omp] path + ten cron jobs (+ the seven
# owner jobs in owner mode); chmod 600 the config; export HOME and the XDG variables to the sandbox.
$B hub & P=$!; until $B status >/dev/null 2>&1; do sleep 0.01; done
sleep 10; grep VmRSS /proc/$P/status
t0=$(awk '{print $14+$15}' /proc/$P/stat); sleep 600; t1=$(awk '{print $14+$15}' /proc/$P/stat)
echo "cpu ticks $((t1-t0)) at $(getconf CLK_TCK) per second"; grep VmRSS /proc/$P/status
kill -TERM $P; wait $P
# incremental debug build
cargo build; touch src/jobs/owner/quota.rs; time cargo build
```

# Milestone 2 slice 2a (Linux x64, debug hub binary, stock OMP 19.1.1, loopback fake model)

From `tests/contract_session.rs::lead_end_to_end_and_resume` (`CATALY_CONTRACT_OMP=1`), 5 owner messages
through `cataly lead send`, figures from `cataly stats --json`:

| Measure | Result | Target |
| --- | --- | --- |
| Hub RSS with the lead attached (OMP excluded), debug build | 18.4-18.8 MiB | ≤ 30 MiB idle + ≤ 1 MiB per session |
| Wake, hub part: internal `deliver` call to `prompt` line written | p50 22.7 ms, max 27.7 ms | p50 ≤ 5 ms, p99 ≤ 25 ms |
| Prompt written to OMP `agent_start` | p50 5.4 ms | (part of end-to-end 150 ms) |
| Hand-off confirmation: prompt written to entry found in the session file | p50 25 ms, max 92 ms (event-driven checks; ms resolution) | none set |

The wake figure misses its target: it includes two `synchronous=FULL` commits (inbox row, then the
delivery record before the write) and one `get_state` readback. Per DESIGN 7.2 the first remedy is to
batch the admission and delivery-record commits into one; not done in this slice. Release-build and
macOS figures are not measured yet. Release binary: 8.6 MB.

## Wake after closing the slice-2a gaps (2026-11-25, Linux x64, release build, stock OMP 19.1.1)

Same scenario in release (`CATALY_CONTRACT_OMP=1 cargo test --release --test contract_session
lead_end_to_end_and_resume -- --nocapture`), now 40 wakes of an idle lead (each message sent after
the previous turn ended and the lead read back idle), figures from `cataly stats --json`. Three runs:

| Measure | Before (slice 2a, debug, 5 sends) | Now, release, 40 wakes per run | Target |
| --- | --- | --- | --- |
| Wake, hub part: `deliver` call to `prompt` line written | p50 22.7 ms | p50 8.5 / 8.6 / 8.7 ms; p99 31.2 / 38.2 / 25.3 ms | p50 ≤ 5 ms, p99 ≤ 25 ms |
| Store commit (`synchronous=FULL`, every commit of the run) | p50 8.3 ms (part A) | p50 8.0 / 8.2 / 8.2 ms | none set |
| Prompt written to OMP `agent_start` | p50 5.4 ms | p50 9.1 / 7.3 / 6.3 ms | part of end-to-end 150 ms |
| Hub RSS with the lead attached (OMP excluded), release | 18.4-18.8 MiB (debug) | 10.3-10.6 MiB | ≤ 30 MiB idle + ≤ 1 MiB per session |

The hot path now holds one durable commit (inbox row and delivery record together) and the `get_state`
readback HOST req. 50 requires runs while that commit waits for its fsync. Before the overlap (one
commit, readback after it) the same release run measured p50 12.0 ms, p99 22.3 ms.

**Floor on this disk.** A bare `synchronous=FULL` WAL commit of one 100-byte row (Python's sqlite3,
SQLite 3.45.1, 300 commits in `/tmp` on the same ext4 NVMe volume, 100 % full): p50 8.35 ms, p99
9.73 ms, minimum 7.69 ms. The fsync alone is above the 5 ms target, so p50 ≤ 5 ms is not reachable
with a durable commit on this machine; the hub adds about 0.2-0.4 ms above that floor at p50 (8.5-8.7
ms against 8.0-8.35 ms). Remaining gap to the target: about 3.5 ms, all of it the fsync. The p99 tail
(25-38 ms, max 60-87 ms) was not broken down; the likely causes are waits behind other work on the
same path (an inbox flush, with its confirmation search and own commit, holding the inbox lock when
the message arrives, or another commit queued ahead in the single writer). Weaker durability would
close the p50 gap; DESIGN 7.2 rules it
out, so the target needs a decision by the owner (accept the disk floor, or a faster disk on the
measurement machine). macOS figures are still not measured.

```sh
cargo build --release
CATALY_CONTRACT_OMP=1 cargo test --release --test contract_session lead_end_to_end_and_resume -- --nocapture
python3 - <<'EOF'   # the floor: 300 single-row FULL commits
import sqlite3, time
c = sqlite3.connect('/tmp/cy-floor.db', isolation_level=None)
c.execute('PRAGMA journal_mode=WAL'); c.execute('PRAGMA synchronous=FULL')
c.execute('CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)')
s = []
for _ in range(300):
    t = time.perf_counter(); c.execute('BEGIN IMMEDIATE')
    c.execute('INSERT INTO t (v) VALUES (?)', ('x' * 100,)); c.execute('COMMIT')
    s.append((time.perf_counter() - t) * 1000)
s.sort(); print(s[149], s[296], s[0])
EOF
```

Release binary after these changes: 8,688,376 bytes.

## Slice 2b: Discord intake to the lead (2026-11-25, Linux x64, release build, stock OMP 19.1.1)

`CATALY_CONTRACT_OMP=1 cargo test --release --test contract_discord -- --nocapture --test-threads=1`:
the hub's lead runs stock OMP on the loopback fake model, intake talks to the fake Discord Gateway and
REST server (`tests/support/discord.rs`); 40 wakes of an idle lead, one slow turn with a follow-up,
5 op-7 reconnects. Figures from `cataly stats --json` at the end of the run.

| Measure | Result | Target |
| --- | --- | --- |
| Wake end to end: Gateway frame read to OMP `agent_start` (`discord_wake_latency`, 41 wakes) | p50 36.1 ms, p95 64.8 ms, p99 77.0 ms, max 109 ms | p50 ≤ 150 ms, p95 ≤ 300 ms |
| Of which hub `deliver` call to `prompt` line written | p50 9.3 ms, p95 15.0 ms | p50 ≤ 5 ms (see the disk floor above) |
| Of which prompt written to `agent_start` | p50 8.5 ms, p95 13.8 ms | none set |
| Reconnect (op 7) to catch-up complete (6 connects) | p50 28.3 ms, max 367 ms (the first connect) | none set |
| Hub RSS, Discord connected and the lead attached, idle after the run (OMP excluded) | 12.5 MiB (13,066,240 bytes) | ≤ 30 MiB idle |
| Release binary (`cargo build --release`, stripped) | 9,700,888 bytes (9.25 MiB; slice 2a: 8,688,376) | none set |

The debug build of the same run: wake p50 40.1 ms, p95 48.6 ms; RSS 22.8 MiB. The remaining wake time
above the hub and OMP parts (about 18 ms at p50) is intake before `Lead::deliver`; it was not broken
down, but it holds two `synchronous=FULL` commits (arrival, then hand-off record) at the 8 ms disk
floor measured above. The release binary grew by
0.97 MiB with the Gateway (tokio-tungstenite, its handshake only), intake, questions and the form
reader. `strings target/release/cataly | grep relay_after_arrival` finds nothing: the crash-test fault
point is compiled out of release builds. macOS figures are not measured.

## Slice 2c: bridge, lead records, owner jobs with Discord (2026-11-25, Linux x64, release build, stock OMP 19.1.1)

`cargo build --release`, then `CATALY_CONTRACT_OMP=1 cargo test --release --test contract_discord --
--nocapture --test-threads=1`: the lead runs stock OMP with the bridge linked, on the loopback fake
model; intake on the fake Discord. `rss_with_discord_lead_and_owner_jobs` configures every owner job
kind live (`github` and `pr` on the fake `gh`, `quota` on stock OMP, `backup` on a local git tree,
`health` with one file check, `items`, `pressure`), answers one request, then reads `cataly stats`
after 10 s idle. Figures from one run:

| Measure | Result | Target |
| --- | --- | --- |
| Hub RSS, Discord connected, the lead attached with its bridge, all seven owner job kinds live, idle (OMP excluded) | 13.0 MiB (13,647,872 bytes) | ≤ 30 MiB idle; owner jobs < 5 MiB |
| Hub RSS, Discord and the lead with its bridge, after 40 wakes, a follow-up and 5 reconnects (OMP excluded) | 13.4 MiB (14,004,224 bytes; slice 2b: 12.5 MiB) | ≤ 30 MiB idle |
| Wake end to end: Gateway frame read to OMP `agent_start` (41 wakes) | p50 38.1 ms, p95 52.3 ms, p99 80.8 ms, max 95.0 ms | p50 ≤ 150 ms, p95 ≤ 300 ms |
| Of which hub `deliver` call to `prompt` line written | p50 9.6 ms, p95 13.2 ms | p50 ≤ 5 ms (disk floor above) |
| Reconnect (op 7) to catch-up complete (6 connects) | p50 21.4 ms, max 361 ms (the first connect) | none set |
| Release binary (`cargo build --release`, stripped) | 10,346,616 bytes (9.87 MiB; slice 2b: 9,700,888) | none set |

The two RSS rows are not a controlled comparison of the owner jobs' share (the runs differ in traffic);
the owner-jobs run with everything live is still 13.0 MiB, and part B's controlled figure for all owner
jobs was 0.8 MiB. The binary grew by 0.62 MiB with the
bridge socket, the hub tool, lead records, the digest and unanswered watch and the question panel.
Debug build of the same runs: 24.3 MiB and 24.4 MiB. macOS figures are not measured.

## Slice 3a: lanes (2026-11-25, Linux x64, release build, stock OMP 19.1.1)

Commands: `cargo build --release`; `CATALY_CONTRACT_OMP=1 cargo test --release --test contract_lanes
-- --nocapture --test-threads=1` (three stock-OMP lanes on the loopback fake model, the bridge
linked; `cataly stats` before the first open and once all three run);
`cargo test --release --test lanes -- --ignored --nocapture oversight_cpu` (eight open lanes on the
fake OMP, hub CPU from `/proc/<pid>/stat` over 120 s idle, scaled to an hour);
`cargo test --release --lib -- --ignored --nocapture spend_read_cost` (a generated 10 MiB session
file, 6,381 entries of 1.5 KB text, half of them billed assistant messages; two runs).

| Measure | Result | Target |
| --- | --- | --- |
| Lane start: spawn to kickoff confirmed in the session file (`lane_start_latency`, 3 starts) | p50 778 ms, max 815 ms (OMP's own start to `ready` is about 490-600 ms, slice 2c) | none set |
| Hub RSS added per running lane (OMP excluded) | 0.49 MiB (12,804,096 to 14,331,904 bytes for three lanes) | none set |
| Hub CPU with 8 open idle lanes (oversight sweeps, watches, spend reads) | 0.6 s per hour (2 clock ticks in 120 s; the tick resolution makes this 0.3 to 0.75 s) | none set |
| Spend reading, 10 MiB session file, first read | 10.3 ms and 10.6 ms | none set |
| Spend reading, one appended call after that (incremental) | 12 µs and 13 µs | none set |
| Release binary (`cargo build --release`) | 11,499,416 bytes (10.97 MiB; slice 2c: 10,346,616) | none set |

The CPU figure is with the fake OMP, which emits no events while idle; the same measure with eight
stock OMP lanes was not taken. macOS figures are not measured.

## Slice 3b: coordination (2026-11-25, Linux x64, release build, stock OMP 19.1.1)

Commands: `cargo build --release`; `cargo test --release --test coord -- --ignored --nocapture
planner_pass` (the hub on the fake OMP with no lead, 100 open items, `cataly lane plan` forced
passes, then a held and a waiting `cataly slot`; the pass time is the hub's own `duration_ms` of
each stored run, read from the store; four runs, the last with 6,005 passes, the others with 55,
305 and 2,005); `CATALY_CONTRACT_OMP=1 cargo test --release --test contract_lanes -- --nocapture
--test-threads=1`.

| Measure | Result | Target |
| --- | --- | --- |
| Planner pass over 100 open items, in the hub (ms resolution; 168 kept runs per measurement) | p50 1-2 ms, max 3 ms | none set |
| `cataly lane plan` round trip (CLI start, socket, the pass and its store commits at the 8 ms disk floor) | 18.7-38.7 ms (first five passes of four runs) | none set |
| Hub RSS idle, then with 100 items (first run) | 12.82 MiB (13,443,072 bytes), then 13.09 MiB | none set |
| Hub RSS, 100 items, 55 planner passes, one slot held and one waiting (first run) | 14.28 MiB (14,970,880 bytes); 14.36 MiB after the slots ended | none set |
| Hub RSS over 6,005 passes, sampled every 500 | 14.96, 15.57, 16.05, 16.47, 16.47, 16.52, 16.52, 16.53, 16.54, 16.71, 16.94, 17.16 MiB; store 5.4 MB plus a 4.1 MB write-ahead log | none set |
| Lane start, spawn to kickoff confirmed (`contract_lanes`, 3 starts) | p50 673 ms, max 793 ms (slice 3a: 778, 815) | none set |
| Hub RSS added per running stock-OMP lane (OMP excluded) | 0.54 MiB (13,422,592 to 15,106,048 bytes for three lanes) | none set |
| Release binary (`cargo build --release`) | 12,226,456 bytes (11.66 MiB; slice 3a: 11,499,416) | none set |

The growth seen in the debug build (27.5 to 29.4 MB with items, passes and slots) is the same
warm-up in release (12.82 to 14.36 MiB): items, the first passes and the slot requests each add a
step, then the steps shrink. Over 6,005 passes the RSS held at 16.47-16.54 MiB from pass 2,005 to
pass 4,505, so a pass retains no memory of its own; 2,500 passes at the 1.1 KB per pass of the early
slope would have added 2.6 MiB. The writer's SQLite page cache (2 MiB) and the two readers' (512 KiB
each) fill as the store grows, which matches the warm-up's size. The later rise of 0.6 MiB over
the last 1,500 passes is not explained; with no lead, every pass leaves one undelivered planner
event in the lead's inbox (6,000 at the end, 250 days of hourly passes), and reading that backlog
is the likely cause [inference, not measured]. Slots added nothing measurable. macOS figures are
not measured.

## Milestone 4: Telegram and Slack (2026-11-25, Linux x64, release build, stock OMP 19.1.1)

Commands: `cargo build --release`; `CATALY_CONTRACT_OMP=1 cargo test --release --test contract_relays
-- --nocapture --test-threads=1` (the lead on stock OMP with the bridge, on the loopback fake model;
Discord, Telegram and Slack all connected to their fakes; one Telegram and one Slack request, then 20
more of each, each answered before the next; `cataly stats` after 10 s idle); the other contract
binaries the same way; `cargo test --release --test coord -- --ignored --nocapture planner_pass`
(one run, 6,005 passes, the carried inbox fix in place).

| Measure | Result | Target |
| --- | --- | --- |
| Telegram intake: poll response read to OMP `agent_start` (`telegram_wake_latency`, 21 wakes) | p50 42.2 ms, p90 46.2 ms, p95 47.3 ms, max 63.6 ms | as Discord: p50 ≤ 150 ms, p95 ≤ 300 ms |
| Slack intake: Socket Mode frame read to OMP `agent_start` (`slack_wake_latency`, 21 wakes) | p50 32.9 ms, p90 39.7 ms, p95 40.6 ms, max 46.7 ms | as Discord |
| Discord intake in the same build (`contract_discord`, 41 wakes) | p50 35.7 ms, p95 46.2 ms, p99 88.6 ms, max 127 ms (slice 2c: 38.1, 52.3) | p50 ≤ 150 ms, p95 ≤ 300 ms |
| Telegram receive lag (`telegram_receive_lag`, 21 readings, nearest rank) | p50 474 ms, p90 887 ms, p99 954 ms, max 988 ms | none set |
| Hub RSS, Discord, Telegram and Slack connected, the lead attached with its bridge, idle after 42 requests (OMP excluded) | 17.85 MiB (18,714,624 bytes) | ≤ 30 MiB idle |
| Hub RSS, Discord only and the lead, after 40 wakes, a follow-up and 5 reconnects, same build | 15.83 MiB (16,601,088 bytes; slice 2c: 13.4 MiB) | ≤ 30 MiB idle |
| Release binary (`cargo build --release`) | 13,080,312 bytes (12.47 MiB; slice 3b: 12,226,456) | none set |
| Planner, 100 items, hub pass time (168 kept runs) | p50 1 ms, max 3 ms (slice 3b: the same) | none set |
| Hub RSS over 6,005 planner passes with no lead, sampled every 500 | idle 13.40 MiB, 100 items 13.79, 5 passes 14.66; then 15.75, 16.21, 16.66, 17.07, 17.10, 17.10, 17.10, 17.13, 17.13, 17.21, 17.47, 17.66 MiB; slots active 17.69, ended 17.77 | none set |

The receive lag is a floor of the fake, not of the hub: Telegram dates are whole seconds and the fake
stamps each message with the current second, so a reading is the time since the start of that second
plus the hub's lag, spread evenly over 0-1 s. On the real service it adds Telegram's own delivery.

Three relays read 2.0 MiB above Discord alone in the same build; the two runs differ in traffic, so
this is not a controlled share (the Telegram poller and Slack socket tasks, their HTTP clients and
two more queues are the likely parts [inference, not broken down]). Discord alone rose 2.4 MiB since
slice 2c (13.4 to 15.8 MiB) over milestones 3 and 4; the lead attached alone reads 14.5 MiB
(`contract_session`, 15,224,832 bytes) and the lane base 13.7 MiB, which points at the hub's base
rather than the relays [inference, not broken down]. The binary grew by 0.81 MiB with Telegram,
Slack, the shared context lookup and the destinations.

**The bounded inbox did not remove the late planner growth.** With no lead, each pass now replaces
the one undelivered planner summary instead of adding one (`inbox_is_bounded_and_keeps_owner_messages`),
yet the run rose 0.53 MiB over the last 1,500 passes (17.13 to 17.66 MiB), as in slice 3b (0.6 MiB),
after the same plateau from pass 2,005 to pass 4,505. So the slice-3b inference that the inbox
backlog caused it is refuted. The store still grows with every pass (the event log takes each
planner event); the SQLite page caches filling with it remain the likely cause [inference, not
measured]. The whole curve sits about 0.5 MiB above slice 3b's, the same step as the hub's base.
macOS figures are not measured.

## Decision 15: early wake (2026-11-25, Linux x64, release build, stock OMP 19.1.1)

Commands: `cargo build --release`; each contract binary alone with `CATALY_CONTRACT_OMP=1 cargo test
--release --test <binary> -- --nocapture --test-threads=1` (figures from the `MEASURE` lines, one run
each). The owner request to the lead is now written while its inbox, delivery and hand-off records
commit; nothing else on the path changed.

| Measure | Before | Now | Target |
| --- | --- | --- | --- |
| Wake, hub part, Discord owner requests: `deliver` call to `prompt` line written (`hub_wake`, 41 wakes) | p50 9.6 ms, p95 13.2 ms (slice 2c) | p50 2.4 ms, p90 10.0 ms, p95 16.9 ms, p99 18.2 ms, max 49.5 ms | p50 ≤ 5 ms, p99 ≤ 25 ms |
| Wake end to end, Discord: Gateway frame read to `agent_start` (41 wakes) | p50 38.1 ms, p95 52.3 ms (slice 2c); p50 35.7 ms, p95 46.2 ms (milestone 4) | p50 33.2 ms, p90 52.1 ms, p95 52.3 ms, p99 53.4 ms, max 81.5 ms | p50 ≤ 150 ms, p95 ≤ 300 ms |
| Wake end to end, Telegram: poll response read to `agent_start` (21 wakes) | p50 42.2 ms, p95 47.3 ms | p50 26.2 ms, p90 39.3 ms, p95 46.9 ms, max 53.1 ms | as Discord |
| Wake end to end, Slack: Socket Mode frame read to `agent_start` (21 wakes) | p50 32.9 ms, p95 40.6 ms | p50 27.6 ms, p90 54.2 ms, p95 59.7 ms, max 140 ms | as Discord |
| Wake, hub part, `cataly lead send` without a request ID (`contract_session`, 40 wakes; commits first as before) | p50 8.5-8.7 ms | p50 8.7 ms, p99 10.6 ms | p50 ≤ 5 ms |
| Store commit (`synchronous=FULL`, every commit of that run) | p50 8.0-8.2 ms | p50 8.3 ms, p99 10.5 ms | none set |
| Hub RSS, Discord connected and the lead attached, after the run (OMP excluded) | 15.83 MiB (milestone 4) | 15.67 MiB (16,433,152 bytes) | ≤ 30 MiB idle |
| Hub RSS, three relays connected, idle after 42 requests | 17.85 MiB | 17.71 MiB (18,567,168 bytes) | ≤ 30 MiB idle |
| Release binary | 13,080,312 bytes | 13,145,944 bytes | none set |

The hub part now meets its p50 target for owner requests: what is left of it is the readback HOST
req. 50 requires and the write itself. The p90-p99 tail (10-18 ms) fits wakes that took the
commit-first path, most likely because the previous wake's delivery was not yet confirmed from the
transcript when the next request came, so the message was stored and sent by the flush, with its
commits [inference, not broken down]. End to end, Discord gained 3-5 ms at p50 rather than the
16 ms the two commits taken off the path would suggest; the arrival commit before the hand-off
(DISC-IN req. 49) is still on the path, and the rest of the intake before `Lead::deliver` (about
20 ms at p50: the round's reads, the lead's inbox lock, which a flush after the previous turn may
hold through its own commit) was not broken down. Telegram gained 16 ms at p50, Slack 5 ms; single
runs, so these are indications, not a controlled comparison. The DESIGN 2.3 budgets stand; the
hub-part row now says which deliveries take the early path. macOS figures are not measured.

## Faster Discord wake (2026-11-25, Linux x64, release build, stock OMP 19.1.1)

Commands: `cargo build --release`; `CATALY_CONTRACT_OMP=1 cargo test --release --test contract_discord
discord_requests_against_stock_omp -- --nocapture --test-threads=1` (41 wakes of an idle lead per run)
and `... --test contract_relays ...` (21 Telegram and 21 Slack wakes per run). The stage table comes
from a throwaway build that stamped each Discord message at every stage (written to a file, not in the
tree), two runs before and two after, p50 / p95 in ms. The machine was shared with three other build
agents (load average 8-10 on 16 cores), so tails are noisy.

| Stage (Discord, live owner message to an idle lead) | Before (a225fb7) | After |
| --- | --- | --- |
| Gateway frame read to parsed and dispatched | 0.05-0.06 / 0.08 | 0.05-0.06 / 0.09-0.10 |
| Owner gate and scope | 0.02 / 0.03-0.04 | 0.02 / 0.03-0.04 |
| Arrival commit (`synchronous=FULL`) | 8.19-8.20 / 8.49-8.76, on the path | 8.24-8.27 / 8.86-10.85, beside the hand-off |
| Round picks the message up (catch-up flag, queue read) | 0.32-0.40 / 0.53 | 0.31-0.34 / 0.49-0.70 |
| Scope check, hand-off record read, question match, route | 0.29-0.36 / 0.50-0.66 | 0.21-0.26 / 0.37-0.48 |
| To the lead's `deliver`, its lock | 0.06-0.07 / 0.09 | 0.04-0.06 / 0.10-0.11 |
| Inbox decision reads (three, now one) | 0.19-0.21 / 0.35-0.41 | 0.18-0.22 / 0.32-0.33 |
| `get_state` readback (HOST req. 50, kept) | 2.03-2.30 / 7.66-10.26 | 1.96-2.28 / 8.06-11.93 |
| Prompt line written | 0.05-0.07 / 0.08-0.11 | 0.05-0.07 / 0.09-0.13 |
| Hub part: frame read to prompt written (sum of the rows above) | about 11.2-11.7 at p50 | about 2.8-3.3 at p50 |
| Prompt written to the hub reading OMP's `agent_start` | 6.01-6.59 / 14.80-17.19 | 7.01-7.70 / 14.25-15.81 |
| Frame read to the hub reading `agent_start` (the wake, end to end) | 17.43-18.42 / 30.68-41.52 | 10.92-10.93 / 28.52-33.31 |
| The hub reading `agent_start` to the relay round seeing it (what the old figure added) | 8.58-9.42 / 35.20-35.26 | no longer counted |

End to end from `cataly stats` (`*_wake_latency`, frame or poll response read to `agent_start`). The
recorded figure now ends when the lead reads `agent_start`; before, it ended when the relay round got
to the event, up to 35 ms later at p95. "Before" below is a225fb7 with only that measuring change
applied, so both columns measure the same thing:

| Measure | Before, same metric (two runs) | After | Target |
| --- | --- | --- | --- |
| Discord, 41 wakes | p50 20.1 / 23.4 ms, p95 28.8 / 125.7 ms | p50 10.7 ms, p95 27.4 ms, p99 29.4 ms, max 40.8 ms (stage runs: p50 11.0 / 10.7, p95 28.6 / 33.3) | p50 ≤ 150 ms, p95 ≤ 300 ms |
| Discord as recorded before the metric change | p50 27.0-29.3 ms, p95 52.6-60.2 ms | (not recorded any more) | |
| Telegram, 21 wakes (arrival still commits first) | p50 22.6 / 20.0 ms, p95 32.2 / 27.1 ms | p50 22.7 / 23.0 ms, p95 46.6 / 49.7 ms | as Discord |
| Slack, 21 wakes (arrival still commits first) | p50 21.1 / 17.1 ms, p95 31.3 / 26.4 ms | p50 21.2 / 22.2 ms, p95 38.8 / 54.4 ms | as Discord |
| Hub part, Discord (`hub_wake`: `deliver` call to prompt written) | p50 2.8 / 3.3 ms | p50 2.4 ms, p95 15.6 ms | p50 ≤ 5 ms, p99 ≤ 25 ms |
| `cataly lead send` without a request ID (`contract_session`, 40 wakes, commits first) | p50 8.7 ms | p50 8.5 ms, p99 13.9 ms | |
| Hub RSS, Discord and the lead, after the run | 16.6-16.8 MB | 16,539,648 bytes (15.8 MiB) | ≤ 30 MiB idle |
| Release binary | 13,145,944 bytes | 13,233,912 bytes | none set |

Discord halves at p50 (the arrival fsync left the path); its p95 is set by OMP's side (readback and
`agent_start` after the write, 14-16 ms at p95) and stayed. Telegram and Slack keep the arrival
commit before the hand-off; their p50 did not move, and their p95 in the
after runs is within the spread of single 21-sample runs on the loaded machine (both after runs are
higher; their path lost two reads and gained nothing [inference: load, not the change]). What is left
of the Discord hub part is the readback (2 ms) and about 1 ms of reads and scheduling. macOS figures
are not measured.

## Memory soak (2026-11-26, Linux x64, release build, fake OMP and fake Discord)

Commands: `cargo test --release --test soak -- --ignored --nocapture --test-threads=1 hub_soak`
(`CATALY_SOAK_ITER=2000`), once against main (dfd9c6f, with the test added) and once with the fixes
that bound the hub's long-lived maps (bridge links, lane turn locks and spend readers, local-only
dialogs, stuck answer sources, Discord channel types, cancelled spend offers, scheduler heap);
`CATALY_PLAN_ROUNDS=24 cargo test --release --test coord --
--ignored --nocapture planner_pass` (12,005 passes) on main. One iteration is an owner message on
Discord answered (every fifth, 400 per run; every 100th an `ask` card, half answered, half expired
after 2 s) or a `cataly lead send` turn; every 50 a lane open, bridge hello, disconnect and close;
every 100 a lead stop and start; every 20 a dry tick of each job kind (probe, cron, github, pr,
quota, backup, health, items, pressure); Gateway reconnects (op 7 and op 9) once the 60 s backoff
reset allows. 2000 iterations take 207-216 s.

| Measure (slope after the first quarter, per 1000 iterations) | Main | Fixed |
| --- | --- | --- |
| RSS = anonymous RSS (file-backed flat at 13.6 MiB) | +3.8 MiB | +4.3 MiB |
| Anonymous RSS at 0 / 1000 / 2000 iterations | 3.5 / 10.3 / 13.5 MiB | 3.2 / 10.6 / 13.2 MiB |
| Anonymous RSS, last 600 iterations | +1.2 MiB | +0.1 MiB |
| Open descriptors | -0.5 (30-31 throughout) | -0.7 (29-30) |
| Threads | 0 (14) | 0 (13) |
| Live tokio tasks | not reported | 0 (46 throughout) |
| `retained` at the end | not reported | bridge links 40, lane spend readers 0, lane turn locks 1 |

Descriptors, threads and tasks are level: no child, connection or task outlives its work across
20 lead restarts, 40 lane cycles and 3 reconnects. The fixed structures are small per entry (a
bridge link, a turn lock, one lane's call IDs), so they do not move these slopes; they grew with
lanes and sessions over the hub's life. The anonymous growth of about 10 MiB in 2000 iterations is
the same with and without the fixes. A 6000-iteration run with the fixes (480 Discord messages,
5520 `lead send` turns, 60 lead restarts, 120 lane cycles, 2699 job ticks, 350 s): anonymous RSS
3.5, 7.9, 11.9, 13.6, 14.4, 14.6, 15.5, 16.5, 17.5, 17.9, 18.4 MiB every 600 iterations; slope
after warm-up +1.2 MiB per 1000 iterations, still falling but not flat; descriptors 30, threads 13,
tasks 46 throughout; `retained` bridge links 64 (the cap), spend readers 0, turn locks 1. The
remaining slope is attributed in "Heap slope" below: freed memory kept by glibc's arenas.

**The planner's late rise.** Main, 100 items, no lead, sampled every 500 passes (anonymous RSS):
4.88 MiB at 505, 6.35 at 2,005, flat 6.46-6.49 from 2,505 to 5,005, then 6.60, 6.77, 6.99, 7.22,
7.35, 7.52, 7.86, 8.16, 8.64, 9.42 at 10,005, then 9.70, 9.76, 9.82, 9.84 at 12,005. File-backed
RSS stays 12.4-12.7 MiB (one step of 1 MiB at 10,005). The database grows 1 KB per pass (0.9 MB to
11.6 MB), the write-ahead log stays at 4.1 MB. So the rise is anonymous heap, it comes in two
steps, each levelling off (the 0.5 MiB of slice 3b and milestone 4 was the start of the second),
and it tracks the database's size rather than the pass count. The writer's page cache (2 MiB) and
the two readers' (512 KiB each) explain the first step; the second (3.4 MiB) exceeds those caps,
which points at the allocator keeping freed memory of transient reads that grow with the tables
[inference, the `MALLOC_ARENA_MAX=1` run that would confirm it was cut short: 6.42 MiB at 2,505,
6.67 at 4,505, 7.74 at 6,505, the same course so far]. No per-pass retention: 2,500 passes added
0.03 MiB on the plateau.

## Heap slope (2026-11-26, Linux x64, release build, fake OMP and fake Discord)

Same soak as above, with the receipt fix (decision 20), so every fifth iteration is a Discord
message with no limit (600 per 3,000 iterations, 2,400 per 12,000; none held a room). Runs went
three to five at a time on the 16-core host. Two tools, both outside the product:

- `CATALY_SOAK_SKIP` leaves parts out (`discord`, `send`, `restarts`, `lanes`, `jobs`,
  `reconnects`); `MALLOC_*` and `GLIBC_TUNABLES` reach the hub.
- A heap sampler preloaded into the hub (`LD_PRELOAD`, a 30-line C file built locally, not in the
  repository; Rust may not use `unsafe` for a counting allocator): every 5 s it appends glibc's
  `mallinfo2()`: `arena` (heap obtained from the system), `uordblks` + `hblkhd` (live, in use) and
  `fordblks` (free but kept).

**Bisection, 3,000 iterations, anonymous RSS slope after the first quarter (KiB per 1000):**

| Run | Slope | Anon RSS at the end |
| --- | --- | --- |
| All parts | 3,231 | 16.5 MiB |
| No Discord | 2,865 | 13.9 MiB |
| No `lead send` turns | 2,470 | 10.8 MiB |
| Messages only (no restarts, lanes, jobs, reconnects) | 2,950 | 13.8 MiB |
| All parts, `MALLOC_ARENA_MAX=1` | 546 | 7.0 MiB |

No part explains it: leaving any one out lowers the slope by 9-24 %, roughly in proportion to the
work left out. One malloc arena removes 83 % of it.

**Live heap against heap held (sampler, 6,000 iterations, all parts):**

| Allocator | Held from the system, at 0 / ¼ / ½ / ¾ / end | Live (in use), same points | Free but kept at the end | Anon RSS at the end |
| --- | --- | --- | --- | --- |
| glibc default (13 threads, one arena each) | 3.7 / 13.1 / 20.7 / 32.6 / 39.9 MiB | 2.3 / 3.6 / 4.0 / 3.6 / 3.6 MiB | 36.3 MiB | 25.2 MiB |
| `MALLOC_ARENA_MAX=1` | 2.4 / 6.4 / 7.0 / 8.9 / 8.9 MiB | 2.3 / 4.0 / 3.4 / 5.1 / 5.4 MiB | 3.5 MiB | 8.3 MiB |

The live heap is level: its mean is 3.9 MiB from the first quarter to the half and 4.3 MiB over the
second half (default), 3.8 and 4.0 MiB (one arena); single samples reach 5.9 MiB with what is in
flight. The 3,000-iteration runs without Discord and without turns show the same (live 3.2 and
4.6 MiB mean over their second half, held 17.6 and 11.5 MiB). So no object accumulates per message,
turn, restart, lane or job tick, and none of the suspects in the brief (event rings and broadcast
channels, transcript reader buffers, the ops panel's recent list, rate-limit and nonce maps,
prepared statements, actor state, Gateway resume buffers, tokio task allocations) retains memory
that grows; all of those would show in the live heap. The growth is freed memory that glibc keeps:
the hub has 13 threads (2 runtime workers, up to 8 blocking-pool threads, the store writer, the
stall watch, main), glibc gives each its own arena, and each arena keeps its own fragmented free
space, which no other thread can use and which is returned only from the top of a heap.

**Longer runs, 12,000 iterations (2,400 Discord messages, 9,600 turns, 120 lead restarts, 240 lane
cycles, 5,400 job ticks, 1,125 s), anonymous RSS every 1,500 iterations:**

- glibc default: 3.4, 13.0, 14.6, 19.6, 22.4, 25.6, 36.9, 39.3, 42.2 MiB; slope +3.4 MiB per 1000
  after warm-up. It does not level off within the run. Each arena should settle at its own plateau,
  so the default allocator probably levels at several times the one-arena figure [inference: 13
  arenas, the one-arena run keeps 3.5 MiB free].
- `MALLOC_ARENA_MAX=1`: 2.5, 6.6, 7.1, 7.8, 8.8, 9.1, 10.4, 10.4, 11.4 MiB; slope +0.48 MiB per 1000,
  in steps (flat 10.4 MiB from 8,700 to 11,100).
- `MALLOC_ARENA_MAX=2` (run alone, 933 s): 3.0, 8.5, 9.8, 10.5, 10.9, 11.9, 13.4, 15.2, 16.3 MiB;
  slope +0.73 MiB per 1000. Sampler: held 13.6 MiB at the half, 17.2 MiB at the end, 13.0 MiB of it
  free; live mean 4.4, 4.6, 4.8 MiB over the second, third and last quarter (+0.06 MiB per 1000,
  within what is in flight).

With one or two arenas what remains grows in steps while the live heap stays level. The soak's data
grows without bound in compressed time: the lead resumes one transcript for the whole run (12,000
turns), and the store keeps every row because its 7-day retention never comes due. Transient reads
of both (the transcript tail of up to 8 MiB for answers, queries over growing tables) grow with
them, which leaves larger free holes [inference, as in the planner run above, where the rise tracked
the tables]. In real use the tail read stops growing at 8 MiB and the tables at 7 days of rows.

**Fix.** The systemd unit now sets `MALLOC_ARENA_MAX=1` for the hub (`sys::service`); children do not
inherit it (their environment is an allow-list). One arena rather than two: 11.4 against 16.3 MiB
after 12,000 iterations, and the run took no longer (1,126 s against 1,124 s for the default, run
side by side); glibc's per-thread caches serve most small allocations without the arena lock. A hub
started by hand gets glibc's default. macOS uses its own allocator; not measured there.

# Milestone 5: voice (2026-11-25)

Linux x64, release builds, 2026-11-25. Hub numbers from
`cargo test --release --features voice --test voice_hub measure -- --ignored --nocapture`: the real
hub against the fake Discord, fake Discord voice server and fake realtime server, five turns of
1.5 s speech plus 2.5 s silence (about 20 s) per transport. RSS added is the peak RSS during the call
minus the RSS of the same hub and fakes with voice disabled.

| Measure | WebSocket | WebRTC | Target |
| --- | --- | --- | --- |
| RSS, voice off | 16.5 MiB | 16.1 MiB | |
| Peak RSS during call | 18.3 MiB | 22.0 MiB | |
| RSS added by the call | 1.8 MiB | 5.9 MiB | ≤ 25 MiB, met |
| CPU during the call | 3.9 % of one core | 6.4 % of one core | |
| Speech end to first audio out (5 turns) | 347-359 ms | 357-386 ms | |

Speech end is the moment the last loud frame was handed to the fake voice server; first audio out
is the first RTP packet the hub sent after it. The fake realtime server answers on its own schedule
with no network or model delay, so the figure is Cataly's own path (receive, decode, send to the
link, receive audio, encode, send) plus the fake's turn detection, not what the owner hears with a
real model.

## Voice binary size (`target/release/cataly`)

| Build | Bytes |
| --- | --- |
| default features | 13,127,192 (12.5 MiB) |
| `--features voice` | 23,035,800 (22.0 MiB) |
| added by `voice` | 9,908,608 (9.4 MiB) |

# Milestone 6: terminal runtime (2026-11-25)

Linux x64, debug test build, stock OMP 19.1.1, loopback fake model, 2026-11-25.

| Measure | Value | How |
| --- | --- | --- |
| Tern wake, hub part (deliver command written to `agent_start` seen through the bridge) | 20.4 ms and 11.4 ms (two runs, one wake each) | `tests/contract_tern.rs`, `stats.agent_start_latency` |
| Tern wake, `cataly lead send` to the model request | 243 ms and 154 ms | same case; includes CLI start, OMP turn set-up and the provider request |
| Tern wake through the hub with the fake bridge (CLI to the deliver command / to `agent_start`) | 13-42 ms / 14-43 ms (3 runs) | `tests/tern.rs`, includes the CLI process start |
| OMP TUI with the bridge connected, 30 s idle after a turn | RSS 342 -> 358 MiB, CPU 2 s (ps, 1 s resolution) | `tests/contract_tern.rs` |
| OMP TUI with the bridge inert (no token, no model), 30 s idle | RSS 323 MiB, CPU 0.57 s | one-off pty run with `/proc` accounting |

The bridge's own share is not isolated: the active run also holds a session and has just finished a turn,
the inert one has neither. The difference (about 20-35 MiB, about 1.4 s CPU per 30 s) is an upper bound for
the bridge plus session plus post-turn TUI work. A tighter split needs both runs with the same session.

## Tern wake by stage and the bridge's own cost

Same machine, debug test build, stock OMP 19.1.1, `tests/contract_tern.rs`. Ten wakes per run; CLI to the
model request is taken from the fake model's own arrival time (no polling in the figure).

| Stage (p50 / max over 10 wakes) | Tern | Headless |
| --- | --- | --- |
| `cataly lead send` start to the model request | 35.4 / 202 ms (other run 51.2 / 225 ms) | 32.7 / 59.7 ms |
| CLI start to CLI exit | 23.4 / 130 ms | 20.4 / 26.5 ms |
| Hub: send handled to command written (`wake_latency`) | 9.5 / 82.9 ms | 9.4 / 12.4 ms |
| Written to `agent_start` (`agent_start_latency`) | 7.4 / 43.0 ms | 7.1 / 14.9 ms |

Finding: the 154-243 ms of the first measurement was one wake per run, the first one in a new OMP process, read
with a 20 ms poll behind a blocking CLI. Steady-state tern wakes match headless (p50 35 vs 33 ms). The cold first
wake is the max in every stage and comes from OMP's first turn in a new process, not from Cataly polling or
serial round trips: the hub's `state` readback already runs in parallel with the inbox commit. Skipping that
readback while the link is fresh was tried and reverted: it broke the `goal get` readback (unit test) and its
effect was not measured. Nothing was removed; no after-figures.

Bridge cost, same session resumed in the stand-in's pty, same workload (ten prompts typed at the terminal,
then 30 s idle), OMP process only, `/proc` (10 ms CPU resolution); run 1 / run 2:

| | Bridge active (hub-hosted tern lead) | Extensions off (`--no-extensions`) |
| --- | --- | --- |
| RSS 5 s after start | 363,884 / 374,192 KiB | 331,564 / 336,284 KiB |
| RSS after the ten turns | 397,292 / 410,160 KiB | 363,228 / 368,664 KiB |
| RSS after 30 s idle | 368,128 / 371,988 KiB | 346,680 / 350,572 KiB |
| CPU of the ten turns | 1,670 / 600 ms | 2,240 / 2,610 ms |
| CPU of 30 s idle | 850 / 940 ms | 960 / 810 ms |

The bridge (with OMP's extension host, which `--no-extensions` also leaves out) costs about 21-38 MiB RSS; its
CPU cost is within run-to-run noise (idle 850-940 vs 810-960 ms per 30 s). Run 2's "with" turns were hub wakes,
not typed prompts; run 1 is the like-for-like pair. Both runs had OMP's first-run setup screen over the TUI.

# Milestone 7: install and release (2026-11-25)

Measured 2026-11-25 on the build host (linux-x64, Linux 7.0). darwin-arm64 was not measured here: no
macOS runner was available; the release workflow builds and checks that target on its own runner.

| What | Value | How |
| --- | --- | --- |
| Binary size, linux-x64 release (`cargo build --release`, host glibc target) | 13 555 064 bytes (12.9 MiB) | `ls -l target/release/cataly` |
| Release build time | 276 s | `/usr/bin/time cargo build --release` (full rebuild) |
| Install through `install.sh` from a local folder | 418 ms | `tests/install.rs` `install_reinstall_and_refusals` (debug variant, 54 MiB binary; hashes, signature, format and version checks included) |
| `update --restart` downtime (old hub's socket closed to new hub answering with the new version) | 86 ms | `tests/install.rs` `restart_switches_back_and_holds_while_busy`, the message `the new hub answered … after the old one closed` |
| Held `--restart` while the lead has a dialog | held after the 60 s stop window, nothing changed, hub still runs | `restart_holds_while_a_session_is_busy` (rollback) |

Not measured: the musl static build (`x86_64-unknown-linux-musl`) size, which the release workflow
produces; the darwin-arm64 size and install time.
