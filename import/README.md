# Vetted imports

Rust the owner wrote himself, checked under the "Vetted imports" rule of `docs/cleanroom.md` and
recorded in `docs/cleanroom-log.md`. These files do not compile as they stand. A build task adapts
a file into the module named below, then deletes it from `import/`. Nothing here is built.

When adapting:

- Use this repository's names, error types (`DESIGN.md` 9.5) and modules. Do not keep a name only
  because the imported file uses it.
- Replace `anyhow` with the module's error enum. Rewrite every slice index as a checked `get`.
- Keep the logic and the tests unless `DESIGN.md` or a spec requires otherwise.
- If a file needs a helper that is not here, write it from the spec, or use the existing module
  that already does the job. Do not add a second implementation of anything.

Adapted and deleted in milestone 1: `m1/rpc_frames.rs` and `m1/rpc_client.rs` (now `omp::rpc`, with
one reader task and a map of pending requests), `m1/fs_helpers.rs` (`sys::fs` already covered it;
its remove-anything helper was not taken because nothing needs it).

Adapted and deleted in milestone 2 slice 2a: `m1/omp_probe.rs` (session launch now `omp::launch`,
identity read and resume check in `host::headless` and `omp::transcript`; the fork-based fresh start
was not taken, `--session-dir` gives a fresh session), `m2/dialogs.rs` (`host::dialogs`, with `form`
dialogs and the spend-offer test added), `m2/session_control.rs` (`host::headless` operations; receipts
are durable rows in `store::host`, keyed per inbox target), `m2/turn_events.rs` (`prompt_result`,
`agent_start` and `message_end` handling in the session actor's event task), `m2/transcript.rs`
(`omp::transcript`, plus the delivery-marker search).

| File | Target module | Known gaps |
| --- | --- | --- |
| `m1/discord_rest.rs` | `relay::discord` REST (milestone 2) | Absorbed in milestone 1: the request policy (now `relay::http` on hyper), message create with parts, enforced nonce and no mentions, channel read, Discord error codes (now `relay::discord`). Left: bot identity and the message-content intent check, history read, replies and their receipts, edit, clear, interaction callbacks, the seen reaction, DM channels, the unknown-message (10008) rule. Use `relay::http` and `relay::discord::Snowflake`; do not bring back a second client. |
| `m1/discord_types.rs` | `relay::discord` types, owner-only button check (milestone 2) | Absorbed in milestone 1: `Snowflake` (now `relay::discord::Snowflake`). Left: message origin, interactions and the owner-only decision check. `ApprovalCard`, `Decision` and `CUSTOM_ID_PREFIX` are undefined: define them from DISC-OUT and the design's form and approval rules. |
| `m2/discord_gateway.rs` | `relay::discord` Gateway | Hand admitted messages to the arrival queue of the relay pipeline. Use `futures-util` only if tokio-tungstenite requires it. First connect uses `v=10&encoding=json`. Log a dropped interaction when the queue is full. |
| `m3/discord_threads.rs` | `relay::discord` thread discovery and parent routes | Route state becomes actor state. Log discovery errors. Mapping a message to its earlier-conversation space is not imported; write it from DISC-IN if that feature is built. |

The lint and release-profile settings the owner used before match `DESIGN.md` 10.3 and are already
in `Cargo.toml`.
