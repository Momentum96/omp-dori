# Provenance

Cataly is licensed under AGPL-3.0-only, and vntrevx also offers it under commercial licences
(`NOTICE`). Offering both is possible only when the copyright holder holds the rights to all of
the code. This page records where the code comes from.

## Where the code comes from

- **Written anew from behaviour specs.** Cataly from 0.2.0 was written from the 13 behaviour
  specs in `spec/`. A spec states what the product does, not how: before it was committed, each
  was audited to remove code, identifiers, file layouts and sentences taken from elsewhere. The
  code, its structure and its names were then written from the specs by agents that had not seen
  the earlier product's code.
- **Reused: only the owner's own Rust.** The only earlier material in Cataly is Rust the owner
  wrote himself: 12 files, 4,215 lines, from commit `bcacce5` of the earlier product's repository.
  Each file passed four checks before it was used:
  1. every line is attributed to the owner in that repository's history;
  2. the file is not a translation of code from a third-party project;
  3. it carries no name, comment or data layout from a third-party project;
  4. a module of `DESIGN.md` needs what it does.

  Every file was then adapted to this repository's names, errors and modules (table below).
- **Nothing third-party from the earlier product.** Nothing from the third-party project the
  earlier product was based on is included in Cataly.
- **Dependencies.** Other people's code enters only as declared dependencies, each listed in
  `docs/third-party.md` with its licence and source.

| Reused file | Now in |
| --- | --- |
| `m1/rpc_frames.rs`, `m1/rpc_client.rs` | `omp::rpc` |
| `m1/fs_helpers.rs` | merged into `sys::fs`, which already covered it |
| `m1/omp_probe.rs` | `omp::launch`, `host::headless`, `omp::transcript` |
| `m2/dialogs.rs` | `host::dialogs` |
| `m2/session_control.rs` | `host::headless` |
| `m2/turn_events.rs` | the session actor's event task in `host::headless` |
| `m2/transcript.rs` | `omp::transcript` |
| `m2/discord_gateway.rs` | `relay::discord::gateway` |
| `m1/discord_rest.rs` | `relay::discord::Rest` |
| `m1/discord_types.rs` | `relay::discord::types` |
| `m3/discord_threads.rs` | `relay::discord::threads` |

## Evidence

- `spec/`: the behaviour specs as they were used.
- `docs/cleanroom-log.md`: a dated record of every spec, audit, reused file and implementation
  task: who did it, what they were allowed to read, and the result, including the details of the
  four checks.
- The git history of this repository.

This record lowers the risk of a copyright dispute; it is not a legal opinion.

## How new code stays clean

For everyone who contributes:

1. **Work from behaviour.** Start from `spec/`, `DESIGN.md` and the public documentation of the
   software Cataly talks to (OMP, Discord, Telegram, Slack, crates, operating systems). Names,
   structure, messages and prompts are written here, for Cataly.
2. **Bring in no code from projects with incompatible licences.** Code, text or data from another
   project may enter only if its licence allows both AGPL-3.0 and commercial distribution, and
   only as a declared dependency or with its licence kept. If you are unsure where something
   comes from, say so in your pull request.
3. **Declare dependencies.** Every dependency is recorded in `docs/third-party.md` with its
   version, licence and source; `cargo deny check licenses` checks it against `deny.toml`.
4. **Sign the CLA.** Every contribution comes under the [Contributor Licence Agreement](../CLA.md),
   so that the commercial licence stays possible.
