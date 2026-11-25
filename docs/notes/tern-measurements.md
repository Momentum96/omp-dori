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
