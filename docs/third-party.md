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
| tokio-tungstenite | 0.30 | MIT | https://crates.io/crates/tokio-tungstenite | `handshake` only: WebSocket framing for the Discord Gateway (`DESIGN.md` 10.2). TLS comes from the existing tokio-rustls (ring) connector in `relay::http`, so no TLS feature is enabled. Brings tungstenite 0.30 (MIT OR Apache-2.0) with sha1, rand, data-encoding, thiserror, utf-8 |
| futures-util | 0.3 | MIT OR Apache-2.0 | https://crates.io/crates/futures-util | `sink`: the stream and sink traits of the WebSocket stream; already a dependency of tokio-tungstenite |
| unicode-width | 0.2 | MIT OR Apache-2.0 | https://crates.io/crates/unicode-width | defaults off: display width for table rendering in `relay::text` (DISC-OUT req. 43) |

Transitive crates come only through these (for example `libsqlite3-sys`, `cc` at build time, `mio`,
`bytes`, `pin-project-lite`, `winnow`, `serde_spanned`, `itoa`, `memchr`, `http`, `http-body`, `httparse`,
`want`, `try-lock`, `atomic-waker`, `regex-automata`, `regex-syntax`, `aho-corasick`, `untrusted`,
`subtle`, `zeroize`). Licences of all 94 locked crates (`cargo metadata --locked`): MIT, Apache-2.0,
ISC, BSD-3-Clause, Zlib, Unlicense OR MIT, Unicode-3.0 (with MIT OR Apache-2.0), CDLA-Permissive-2.0
and Apache-2.0 WITH LLVM-exception; all within the allow list of `DESIGN.md` 10.2. `cargo deny` itself
is not yet a CI step; `deny.toml` holds its licence policy.

## Voice build only (`cataly-voice`, feature `voice`)

These link only into builds with the `voice` feature (`DESIGN.md` 10.1); a build without it carries
none of them. All are used unmodified from crates.io.

| Crate | Version | Licence | Source | Features enabled and why |
| --- | --- | --- | --- | --- |
| davey | 0.2.0 | MIT | https://crates.io/crates/davey (https://github.com/Snazzah/davey) | defaults off: Discord's DAVE end-to-end encryption (MLS group, key packages, commits and welcomes, frame encryption). Brings openmls 0.8 and openmls_rust_crypto 0.5 (MIT) and RustCrypto crates. |
| hpke-rs, hpke-rs-crypto, hpke-rs-rust-crypto | 0.6.1 | MPL-2.0 | https://crates.io/crates/hpke-rs (source: https://github.com/cryspen/hpke-rs; the exact sources ship in each crate on crates.io) | transitive through davey -> openmls_rust_crypto: HPKE for MLS. MPL-2.0 is file-level copyleft; the files are used unmodified, so the obligation is to keep their source available, which the links above do. The only licence outside `DESIGN.md` 10.2's list; `deny.toml` allows it for these three crates alone. |
| audiopus | 0.3.0-rc.0 | ISC | https://crates.io/crates/audiopus | `coder`: Opus encode and decode of Discord audio |
| audiopus_sys | 0.2.2 | ISC; bundled libopus BSD-3-Clause | https://crates.io/crates/audiopus_sys (libopus: https://opus-codec.org) | `static`: libopus is built from the bundled source and linked statically, so no system library is needed |
| str0m | 0.26.0 | MIT OR Apache-2.0 | https://crates.io/crates/str0m | `rust-crypto`, defaults off: sans-I/O WebRTC (ICE, DTLS, SRTP, SCTP data channel) for the realtime model's WebRTC transport. Brings dimpl 0.9 (MIT OR Apache-2.0) for DTLS, which in turn brings aws-lc-rs. |
| aws-lc-rs, aws-lc-sys | 1.19.0, 0.47.0 | ISC AND (Apache-2.0 OR ISC); aws-lc-sys adds Apache-2.0, MIT, BSD-3-Clause and OR-choices including MIT-0 | https://crates.io/crates/aws-lc-rs (https://github.com/aws/aws-lc-rs) | transitive through str0m -> dimpl: AWS-LC (C) built at compile time. Every licence in its expression is on the allow list. |
| chacha20poly1305 | 0.10.1 | Apache-2.0 OR MIT | https://crates.io/crates/chacha20poly1305 | `alloc`, defaults off: Discord's `aead_xchacha20_poly1305_rtpsize` transport mode, the fallback when a voice server offers no AES-GCM (AES-256-GCM comes from ring) |

The voice tests (`tests/voice_hub.rs`, `crates/voice/tests/transports.rs`) also use audiopus,
audiopus_sys and str0m as dev-dependencies of `cataly` for the fake realtime and Discord voice
servers; dev-dependencies never enter a release build.

## Dev-dependencies

| Crate | Version | Licence | Source | Features enabled and why |
| --- | --- | --- | --- | --- |
| hyper | 1.13 | MIT | https://crates.io/crates/hyper | `server`, `http1`: the fake Discord REST server in `tests/support/discord.rs` (`DESIGN.md` 10.2 dev list) |

The fake `gh` and fake `omp` in `tests/support/` are POSIX shell scripts written here.
