# Measurements: milestone 1

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

Measured by the part-B build agent (BuildMilestone1B) on this machine before `relay::http` was written,
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
# omp copied from tests/support with fixture files; config = [omp] path + ten cron jobs (+ the seven
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
