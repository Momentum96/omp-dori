# Live voice runs and spend

The owner approved 0.50 USD in total for live voice testing (2026-11-26). Costs are what the live
test's own ledger reports (billed speech seconds at the test's speech price plus recorded backend
spend); a run that panicked before printing its cost is estimated.

| Time (UTC) | What ran | Cost (USD) | Running total (USD) |
| --- | --- | --- | --- |
| 2026-11-26, before this log | test recording | 0.0010 | 0.0010 |
| 2026-11-26, before this log | `live_voice` WebSocket: passed | 0.0294 | 0.0304 |
| 2026-11-26, before this log | `live_voice` WebRTC: failed, 0 frames played (cost not printed; estimate) | ≤ 0.0300 | ≤ 0.0604 |
| 2026-11-26 21:07 | `live_voice` WebRTC only, traced: exchange passed (ledger 0.0155); low-cap session held at once, cap notice missing (cost not printed; 15 s start billing estimated) | ≈ 0.0310 | ≤ 0.0914 |
| 2026-11-26 21:16 | `live_voice` WebRTC only, traced, with the start fix: passed (POSTs answered after 1.3 s and 2.4 s, `session.started` 0.2 s and 0.8 s later) | 0.0329 | ≤ 0.1243 |
| 2026-11-26 21:20 | `live_voice` WebRTC only, final code without trace: passed (one exchange, one pass-to-lead with receipt, 373 frames played, low-cap stop with notice) | 0.0350 | ≤ 0.1593 |

This fix's live runs (21:07, 21:16, 21:20) used about 0.099 USD of the 0.35 USD it was allowed.
