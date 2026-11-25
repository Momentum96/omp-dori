# Milestone 7 measurements (branch feat/release)

Measured 2026-11-25 on the build host (linux-x64, Linux 7.0). darwin-arm64 was not measured here: no
macOS runner was available; the release workflow builds and checks that target on its own runner.

| What | Value | How |
| --- | --- | --- |
| Binary size, linux-x64 release (`cargo build --release`, host glibc target) | 13 555 064 bytes (12.9 MiB) | `ls -l target/release/cataly` |
| Release build time | 276 s | `/usr/bin/time cargo build --release` (full rebuild) |
| Install through `install.sh` from a local folder | 418 ms | `tests/install.rs` `install_reinstall_and_refusals` (debug variant, 54 MiB binary; hashes, signature, format and version checks included) |
| `update --restart` downtime (old hub's socket closed to new hub answering with the new version) | 86 ms | `tests/install.rs` `restart_switches_back_and_holds_while_busy`, the message `the new hub answered … after the old one closed` |
| Held `--restart` while the lead has a dialog | held after the 60 s stop window, nothing changed, hub still runs | `restart_holds_while_a_session_is_busy` (rollback) |

Not measured: the musl static build (`x86_64-unknown-linux-musl`) size, which the release workflow
produces; the darwin-arm64 size and install time.
