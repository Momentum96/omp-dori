# Third-party code

Every dependency of the `cataly` crate, with licence and source. All are declared crates from crates.io;
no third-party source is copied into this repository. Allowed by `DESIGN.md` 10.2.

| Crate | Version | Licence | Source | Features enabled and why |
| --- | --- | --- | --- | --- |
| tokio | 1.54 | MIT | https://crates.io/crates/tokio | `rt-multi-thread`, `net`, `process`, `time`, `signal`, `sync`, `io-util` as in 10.2; `macros` for `select!` (compile-time only, see `docs/build-notes.md`). `fs` is not enabled: nothing uses it yet. |
| serde | 1.0 | MIT OR Apache-2.0 | https://crates.io/crates/serde | `std`, `derive`: wire and record types |
| serde_json | 1.0 | MIT OR Apache-2.0 | https://crates.io/crates/serde_json | `std`: control protocol, event log |
| rusqlite | 0.37 | MIT (bundled SQLite: public domain) | https://crates.io/crates/rusqlite | `bundled`: no system SQLite |
| ring | 0.17 | Apache-2.0 AND ISC | https://crates.io/crates/ring | none beyond defaults off: SHA-256 (condition-job output, config and receipt digests); `rand::SystemRandom` (the operating system's secure generator) for the secure tokens of the session host: single-use launch tokens (`CATALY_LAUNCH`), session handles and delivery IDs (`host::random_hex`) |
| toml | 0.9 | MIT OR Apache-2.0 | https://crates.io/crates/toml | `std`, `parse`, `serde`: reading the config (no writer) |
| jiff | 0.2 | Unlicense OR MIT | https://crates.io/crates/jiff | `std`, `tzdb-zoneinfo`: IANA zones from the system zoneinfo, DST gaps and folds |
| rustix | 1.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://crates.io/crates/rustix | `std`, `fs`, `process`: `statvfs`, `getuid`, `kill(SIGTERM)` to own children, `O_NOFOLLOW` |
| hyper | 1.13 | MIT | https://crates.io/crates/hyper | `client`, `http1`: the HTTP/1.1 client under `relay::http`. Replaces reqwest by the measurement `DESIGN.md` 10.2 asked for (`docs/measurements.md`). |
| hyper-util | 0.1 | MIT | https://crates.io/crates/hyper-util | `tokio`: the tokio I/O adapter for hyper; no pool or legacy client |
| http-body-util | 0.1 | MIT | https://crates.io/crates/http-body-util | defaults off: a full request body and a size-limited response body (2 MiB cap) |
| tokio-rustls | 0.26 | MIT OR Apache-2.0 | https://crates.io/crates/tokio-rustls | `ring`, `tls12`: TLS on tokio streams through rustls with the ring provider (`DESIGN.md` 10.2); brings rustls 0.23 (Apache-2.0 OR ISC OR MIT), rustls-webpki (ISC), rustls-pki-types (MIT OR Apache-2.0) |
| webpki-roots | 1.0 | CDLA-Permissive-2.0 | https://crates.io/crates/webpki-roots | defaults off: the Mozilla root set compiled in, so no system trust store is read |
| regex | 1.13 | MIT OR Apache-2.0 | https://crates.io/crates/regex | `std`, `perf` only (no Unicode tables): owner secret patterns of the backup job, linear time. Unicode classes in a pattern are refused as a config error. |
| unicode-width | 0.2 | MIT OR Apache-2.0 | https://crates.io/crates/unicode-width | defaults off: display width for table rendering in `relay::text` (DISC-OUT req. 43) |

Transitive crates come only through these (for example `libsqlite3-sys`, `cc` at build time, `mio`,
`bytes`, `pin-project-lite`, `winnow`, `serde_spanned`, `itoa`, `memchr`, `http`, `http-body`, `httparse`,
`want`, `try-lock`, `atomic-waker`, `regex-automata`, `regex-syntax`, `aho-corasick`, `untrusted`,
`subtle`, `zeroize`). Licences of all 94 locked crates (`cargo metadata --locked`): MIT, Apache-2.0,
ISC, BSD-3-Clause, Zlib, Unlicense OR MIT, Unicode-3.0 (with MIT OR Apache-2.0), CDLA-Permissive-2.0
and Apache-2.0 WITH LLVM-exception; all within the allow list of `DESIGN.md` 10.2. `cargo deny` itself
is not yet a CI step.

## Dev-dependencies

| Crate | Version | Licence | Source | Features enabled and why |
| --- | --- | --- | --- | --- |
| hyper | 1.13 | MIT | https://crates.io/crates/hyper | `server`, `http1`: the fake Discord REST server in `tests/support/discord.rs` (`DESIGN.md` 10.2 dev list) |

The fake `gh` and fake `omp` in `tests/support/` are POSIX shell scripts written here.
