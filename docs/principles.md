# Design principles

Every design and review in Cataly is judged against these principles, in this order.

## 1. Thin over OMP

- OMP is the engine. A new OMP release must work without anyone touching Cataly, unless
  OMP itself changes its public surfaces.
- Use only OMP's stable public surfaces: the rpc-ui protocol, the extension API, session
  files and CLI flags. Never depend on OMP internals. Carry no patch to OMP.
- The OMP extension is TypeScript, because OMP loads TypeScript. Keep it as small as
  possible. Logic lives in the Rust binary.
- CI runs against the latest OMP release.

## 2. Light

- No unused feature, code path, setting or compatibility shim.
- One implementation per concern.
- Few long-lived processes: one Rust daemon where possible. Measure memory before and
  after every change.

## 3. Fast

- Start-up, wake (messenger message to session reaction) and command latency are as low
  as possible.
- Measure first. Optimise nothing without a measurement.

## 4. Stable

- No panics in production paths. Every external call (OMP, network, file system,
  subprocess) has a bound, a timeout and a recovery path.
- Fail closed when safety is at stake.
- Every behaviour ships with tests on Linux and macOS.

## Fixed constraints

- Bun stays on the machine because OMP runs on it. Every Cataly-owned process and command
  is Rust.
- Releases target linux-x64 and darwin-arm64. macOS is first class.
