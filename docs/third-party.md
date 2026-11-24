# Third-party code

Every dependency of the `cataly` crate, with licence and source. All are declared crates from crates.io;
no third-party source is copied into this repository. Allowed by `DESIGN.md` 10.2.

| Crate | Version | Licence | Source | Features enabled and why |
| --- | --- | --- | --- | --- |
| tokio | 1.54 | MIT | https://crates.io/crates/tokio | `rt-multi-thread`, `net`, `process`, `time`, `signal`, `sync`, `io-util` as in 10.2; `macros` for `select!` (compile-time only, see `docs/build-notes.md`). `fs` is not enabled: nothing uses it yet. |
| serde | 1.0 | MIT OR Apache-2.0 | https://crates.io/crates/serde | `std`, `derive`: wire and record types |
| serde_json | 1.0 | MIT OR Apache-2.0 | https://crates.io/crates/serde_json | `std`: control protocol, event log |
| rusqlite | 0.37 | MIT (bundled SQLite: public domain) | https://crates.io/crates/rusqlite | `bundled`: no system SQLite |
| ring | 0.17 | Apache-2.0 AND ISC | https://crates.io/crates/ring | none beyond defaults off: SHA-256 of condition-job output |
| toml | 0.9 | MIT OR Apache-2.0 | https://crates.io/crates/toml | `std`, `parse`, `serde`: reading the config (no writer) |
| jiff | 0.2 | Unlicense OR MIT | https://crates.io/crates/jiff | `std`, `tzdb-zoneinfo`: IANA zones from the system zoneinfo, DST gaps and folds |
| rustix | 1.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://crates.io/crates/rustix | `std`, `fs`, `process`: `statvfs`, `getuid`, `kill(SIGTERM)` to own children, `O_NOFOLLOW` |

Transitive crates come only through these (for example `libsqlite3-sys`, `cc` at build time, `mio`,
`bytes`, `pin-project-lite`, `winnow`, `serde_spanned`, `itoa`, `memchr`). `cargo deny` with the licence
list of `DESIGN.md` 10.2 is a CI step of `docs/plan.md`, not run in this slice.

No dev-dependencies: tests use only the crates above and the standard library.
