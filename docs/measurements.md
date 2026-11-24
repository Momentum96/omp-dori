# Measurements: milestone 1, part A

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

Not measured in this slice (part B or CI): RSS with relays and all owner jobs, CPU per idle hour, the
reqwest-versus-hyper comparison, macOS figures, incremental debug build time.
