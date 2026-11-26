# Clean-room rules for Cataly

Cataly (from 0.2.0) is written from behaviour specs only. It contains no code, structure, names or
prose from the earlier Cataly tree or from the project that tree was copied from.

## Rules for everyone who writes in this repository

1. Read only this repository and public documentation of third-party software (OMP,
   Discord, Telegram, Slack, crates, operating systems).
2. Never open the earlier Cataly repository, its git history, its installed copies, or the
   analysis workspace. If you have seen that material in this session, do not write code
   or design here; hand the work to an agent that has not.
3. Work from `spec/`. A spec states behaviour. Every name, module boundary, data layout,
   command, flag, message text and prompt in this repository is designed here, for Cataly.
4. Third-party code enters only as a declared dependency or under a licence that allows
   commercial use, and is recorded in `docs/third-party.md` with its licence and source.
5. If a spec seems to carry code or wording from somewhere else, stop and report it.
   Do not implement from it.

## How a spec gets here

1. An analysis agent studies the earlier product and writes a behaviour spec outside this
   repository.
2. An audit removes anything that is expression rather than behaviour: code, identifiers,
   file layouts, copied sentences.
3. The audited spec is committed under `spec/`, and the audit is recorded in
   `docs/cleanroom-log.md`.

## Vetted imports

The owner wrote all Rust in the earlier tree himself. Such a file may be brought into this
repository only by the coordinator, and only when all of these hold:

1. Every line of the file is attributed to the owner in the earlier repository's history.
2. The file is not a translation of code that came from the upstream project.
3. The file carries no name, comment or data layout taken from the upstream project.
4. The design in `DESIGN.md` needs what the file does, so it replaces work, not adds to it.

Imported files arrive under `import/`, each listed in `docs/cleanroom-log.md` with its
source path, commit and the result of the four checks. Build agents then adapt them to
this repository's names and structure. Everything else is still written from `spec/`.

## Record

`docs/cleanroom-log.md` lists, for every spec and every implementation task, which role
produced it, what that role was allowed to read, and the date. Keep it accurate; it is the
evidence that the wall held.

## Limits

This process lowers the risk of a copyright claim. It is not a legal guarantee. Before the
product is sold, a lawyer should review this record and the licence of the earlier
project.
