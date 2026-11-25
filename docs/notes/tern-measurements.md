# Milestone 6 measurements (terminal runtime)

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

## Follow-up (2026-11-25): wake by stage and the bridge's own cost

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
