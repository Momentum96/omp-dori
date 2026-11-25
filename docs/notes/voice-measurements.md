# Milestone 5: voice measurements

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

## Binary size (`target/release/cataly`)

| Build | Bytes |
| --- | --- |
| default features | 13,127,192 (12.5 MiB) |
| `--features voice` | 23,035,800 (22.0 MiB) |
| added by `voice` | 9,908,608 (9.4 MiB) |
