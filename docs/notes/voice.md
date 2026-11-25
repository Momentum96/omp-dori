# Milestone 5: voice, hub side (build notes)

The `cataly-voice` crate (commits 6fe06e2, 99ac9b9) holds the call; this part connects it to the hub
(`DESIGN.md` 5.2, 10.1). Build agent VoiceFinish.

## Shape

| Part | Where | What it does |
| --- | --- | --- |
| `VoiceHost` implementation | `host::voice` (`HubVoice`) | network through `relay::http` (`connect`, one-attempt `call` with `Kind::Once` for paid POSTs); op 4 onto the Gateway; pass-to-lead through `Lead::deliver` and `Lead::receipt`; status reads from the lanes; note posts through the Discord client (`Rest::post`, outgoing checks, enforced nonces); ledger, hold and reply queue as `coord` records |
| Voice task | `hub::voice` (`Voice`) | supervised task `voice`: one call at a time while the settings are valid and the key reads; stays `off` naming the field otherwise; restarts the call when the voice settings change; answers `voice.status`, `voice.restart`, `voice.reply` |
| Gateway tap | `relay::discord::tap` | the relay's only voice seam, with no voice types: extra intents (guilds, voice states) when the config wants voice, the dispatches voice reads (`GUILD_CREATE`, `VOICE_STATE_UPDATE`, `VOICE_SERVER_UPDATE`), READY/RESUMED and link-down facts, and a queue of frames (op 4) the Gateway sends once established |
| Gateway ops | `relay::discord::gateway` | `Gateway.ops`: the connection holds the queue's receiver while it lives and sends queued frames after READY or RESUMED |
| Commands | `cli`, `proto`, `hub::dispatch` | `cataly voice status`, `cataly voice restart`, `cataly voice reply <request> <reply> --subject <s>`; a build without voice answers status with why voice is off and refuses the others |

Records in the `coord` table (no migration): `voice_day` (id `YYYY-MM-DD` in the configured zone;
speech seconds, sessions, spend, cap-notice flag), `voice_hold` (id `voice_hold`; the reason),
`voice_reply` (id: a zero-padded arrival number; request ID, subject, text, bytes spoken). A row that
does not read is corruption: voice stops and the row is never rewritten (compare-and-swap on the
row's version for every write).

## Choices

- **Voice needs Discord intake.** Voice rides the one Gateway connection (VOICE-03), which runs only
  while intake is configured; without `[discord] owner` and `channels` voice stays off and names
  `discord.channels`. A missing or unreadable key file names `voice.key_file` (default
  `<config dir>/secrets/voice-key`, owner-only, like the relay tokens).
- **Gateway facts are kept for the next call.** READY's bot ID, the guild snapshot (channel types and
  voice states, updated by voice-state updates) are cached in `hub::voice`, and a call started after
  the connection came up gets them first. Toggling voice changes the intents, so intake reconnects.
- **Pass-to-lead** (VOICE req. 73-75): refused with the reason unless the lead is live with no open
  dialog, input not blocked, not quota-blocked and not faulted; sent once through the lead's inbox
  (sender `voice`, owner's, urgent, the voice request ID as the caller request ID); `Received` only
  when the receipt reaches `delivered` within 10 s and the lead session ID is unchanged; a failed
  or duplicate admission is `Refused`; everything else, including an inbox error after admission,
  is `Unproven` and never resent.
- **Replies** (VOICE-17): `cataly voice reply` accepts only voice request IDs (`v...`), at most 64
  waiting replies, 8 KiB of text and a 120-character subject.
- **Status reads** (VOICE req. 69): lanes from the lane records with the live session's status and
  its time, plus the lead as the one session that is not a lane; reports: the newest report of up to
  8 open lanes, newest first, each cut to `cataly_voice::REPORT_MAX`.
- **Restart** (`cataly voice restart`) deletes the hold record, then tells a running call or wakes a
  stopped task; a hold survives a hub restart because it is a record.

## Fixes found on the way

- **The Discord Gateway task outlived its intake run.** `Intake::serve` spawned the connection and
  never stopped it, so a config change left the old connection running beside the new one. The task
  is now aborted when `serve` returns (`AbortOnDrop`). The op queue's lock would otherwise have
  stayed with the stale connection.
- **Plaintext with DAVE pending.** With DAVE negotiated but its group not yet formed, the receive
  path treated the call as unencrypted and accepted plaintext indefinitely. It now accepts
  unencrypted owner frames only within 30 s of the last encryption-group change (VOICE req. 58), and
  starting a fresh DAVE group counts as a change (`crates/voice/src/discord/conn.rs`).

## Finding: DAVE is mandatory

Discord accepts only end-to-end encrypted (DAVE) voice connections since 2026-03-01 (Discord
developer documentation, "Voice", DAVE section). A client that does not offer a DAVE protocol version
in Identify (`max_dave_protocol_version`) is closed with code 4017. `cataly-voice` therefore always
offers davey's version and treats 4017 as fatal with its meaning; there is no unencrypted fallback.
The fake voice server in `tests/support/discord_voice.rs` can announce DAVE without forming a group,
which is how the plaintext window is tested end to end.

## Choice: the GPT-Live API

The realtime link uses OpenAI's GPT-Live API (`/v1/live/sessions`, model `gpt-live-1`, generally
available since 2026-09-10; https://developers.openai.com/api/docs/models/gpt-live-1) rather than
`/v1/realtime`:

- it is built around a speech model that delegates facts, tools and work to a backend model, which is
  VOICE-11 and VOICE-13 as specified (delegated backend, tool continuation);
- one endpoint serves both WebSocket and WebRTC (owner decision 11), so the session logic is written
  once behind `RealtimeLink`;
- billing is per second of session time ($0.05 per active minute for `gpt-live-1`, not rounded up)
  plus the backend model's tokens, which the ledger records as speech seconds and estimated spend.

WebRTC standby ($0.02 per minute while inference is paused) is not used: provider standby stays
dropped (`DESIGN.md` 5.2); idle close plus wake on speech covers it.

## Tests

- `tests/voice_hub.rs` (voice builds): through the real hub, the fake Discord (REST, Gateway with
  voice-state and voice-server answers to op 4), the fake Discord voice server (voice WebSocket v8,
  UDP with IP discovery and `aead_aes256_gcm_rtpsize`), the fake realtime server and the fake OMP
  lead. Check 1 (missing setting and missing key named; valid settings start and join; corrupt
  ledger stops voice and is never rewritten), check 2 (owner-only receive and subscription, a dead
  stream resubscribed, the plaintext window under DAVE), check 3 (wake with lead-in, pacing,
  barge-in), checks 4 and 8 (received, unproven and refused through the real lead; both transports
  agree), check 5 (reply queued by command, counted only when proven, kept otherwise), check 6
  (hold across a hub restart, notices, no paid retry, `voice restart`); every call-level check over
  both transports. An ignored `measure_call_memory_and_cpu` gives the measures.
- `crates/voice/tests/transports.rs`: the call against a fake host over both transports (checks 1,
  3-6, 8). `tests/live_voice.rs`: the owner-gated live test, ignored, never run here.

## Not done here

- The live test (`tests/live_voice.rs`) is written and was not run: it needs the owner's key,
  approved limit and a recording of the owner's voice (no speech synthesiser on the build machine).
- `cargo deny` is not installed on the build machine; `deny.toml` was written from the licence list
  of `cargo metadata` and not run.
- Linux only so far; the macOS run belongs to CI.
