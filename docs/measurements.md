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
