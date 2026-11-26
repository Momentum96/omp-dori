# Contributing

This repository holds Cataly's source, its releases and its front page (`README.md`).

## Build from source

You need the Rust toolchain from `rust-toolchain.toml` and [Bun](https://bun.sh) for the OMP
extension in `extension/`.

```sh
cargo build --release                    # add --features voice for Discord voice
```

The binary lands in `target/release/cataly`. `sh scripts/third-party-notices.sh` writes the
third-party licences, `THIRD-PARTY-NOTICES.txt`, that every release ships per target as
`THIRD-PARTY-NOTICES-<target>.txt`.

## Build and test

You need the Rust toolchain from `rust-toolchain.toml` and [Bun](https://bun.sh) for the OMP
extension.

```sh
cargo build
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --no-fail-fast
cargo test --locked --no-fail-fast --workspace --features voice   # the voice build
cd extension && bun install --frozen-lockfile && bun run typecheck && bun test
```

`cargo deny check` checks advisories and dependency licences against `deny.toml`. Every new
dependency is recorded in `docs/third-party.md` with its licence and source. Read
`docs/cleanroom.md` and `docs/principles.md` before you write code.

An optional pre-commit hook runs the format check and both clippy builds before each commit:

```sh
git config core.hooksPath .githooks
```

## Continuous integration

`.github/workflows/ci.yml` runs on every pull request, every push to `main`, nightly and on a
manual dispatch, on the four release targets: `linux-x64` (`ubuntu-latest`), `linux-arm64`
(`ubuntu-24.04-arm`), `darwin-arm64` (`macos-latest`) and `darwin-x64` (`macos-15-intel`). A
dispatch takes `os` = `linux`, `macos` or `all` (the default) to pick the Linux or macOS rows.
Every job sets up through `.github/actions/setup`: the toolchain from `rust-toolchain.toml`, the
Cargo cache per OS, CPU, target and `Cargo.lock`, and Bun with its cache per `extension/bun.lock`.

- `build (<target>)`: the format check, clippy and the tests with and without voice, the Bun
  tests and type check of the extension, both release builds, binary size and idle memory.
- `cargo deny`: advisories, bans, licences and sources.
- `contract against OMP (<target>)`: every `contract*` test against stock OMP (the pinned
  release; the latest release nightly and on a dispatch).
- `release smoke (<target>)`, on pushes to `main` and dispatches only: the release binary built
  with a throwaway key, installed by `install.sh` from a signed local folder into an empty home,
  then `cataly version` and `cataly health`.

A newer push to a pull request cancels its running checks. On macOS the tests' waits for
something to happen are three times as long (`CATALY_TEST_PATIENCE`, read by `tests/support`);
what they check is the same on every target.

`.github/workflows/build.yml`, dispatched by hand, builds the release binary of every target with
the release key, the commit and its time, and uploads each as `cataly-<version>-<target>` for
`scripts/release.sh --add`. Windows is not a target.

## Licence and CLA

Cataly is licensed under AGPL-3.0-only (see `LICENSE` and `NOTICE`); vntrevx also offers it
under commercial licences. So that this stays possible, every contribution needs the
[Contributor Licence Agreement](CLA.md). Accept it by ticking "I accept the CLA in CLA.md" in
the pull request template, or by writing the sentence in `CLA.md` in your pull request. Pull
requests without it are not merged.
