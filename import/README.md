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

Adapted and deleted in milestone 2 slice 2b: `m2/discord_gateway.rs` (`relay::discord::gateway`:
the connection loop behind a `Handler` trait, TLS through `relay::http::connect`, `futures-util` for the
WebSocket stream; admitted owner messages go to the durable arrival queue before the next frame),
`m1/discord_rest.rs` (the rest of it on `relay::discord::Rest`: bot identity and the message-content
intent check, history and single-message reads, linked reply parts and the 10008 rule, edits, clearing
buttons by an edit, interaction callbacks, the seen reaction; the DM-channel call was not taken because
the owner's DM channel is listed by ID like any other room), `m1/discord_types.rs`
(`relay::discord::types`: message gate, normalisation and button presses; the owner-only decision with
its stamp is in `relay::discord::questions`, which defines the approval card from DISC-OUT req. 81-83).

Adapted and deleted in milestone 3 slice 3a: `m3/discord_threads.rs` (`relay::discord::threads`:
thread discovery and parent routes for lane threads; mapping a message to its earlier-conversation
space was not imported).

Nothing else is left in `import/`: every vetted file has been adapted and deleted; only this README
remains.

The lint and release-profile settings the owner used before match `DESIGN.md` 10.3 and are already
in `Cargo.toml`.
