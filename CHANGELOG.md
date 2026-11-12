# Changelog

## 0.1.0 — Linux x64 preview (`v0.1.0`, prerelease)

This is the first public-release interface for the OMP port. Live consumer support is scoped to Linux x64, herdr and Discord. The tested clean-consumer baseline is Ubuntu 24.04 x64 with Bun 1.4.4, native OMP 18.18.8 and herdr 0.9.5; Bun and OMP versions are also the minimum requirements. Newer versions, other Linux distributions, macOS, hc and other messengers do not inherit an unperformed live consumer test from that baseline. See the [installation and support guide](docs/releases.md) for the support boundary and the canonical release path.

### Release installation and recovery

- The root `install.sh` installs an exact verified release instead of cloning or pulling arbitrary `main`. Select this preview explicitly with `--version v0.1.0` or `DORI_RELEASE=v0.1.0`; default latest selects stable releases only.
- Fixed assets are `omp-dori-v0.1.0.tar.gz`, `release-manifest.json`, `SHA256SUMS` and `install.sh`. Version/tag, commit/tree, archive digest and the extracted public source manifest are checked before activation. Unsafe archive paths, links and special files are rejected.
- The explicit launcher is `~/.dori/bin/dori`. CLI, skill and extension use one active `~/.dori/src` pointer, with verified previous releases retained for rollback. Existing configuration, authentication, work and sessions are preserved.
- `dori update check|install|status|rollback` is the consumer release API. `--version` selects an exact release; `check` and `install` also accept `--from DIST` for local assets. First-time pinned public omp-ulw retrieval can still require anonymous network access.
- Consumer-managed dependency cache paths resolve from the executing release's own `omp-ulw.lock`, including after update or rollback, without rewriting saved configuration. Explicit dependency paths outside that managed cache remain user-owned.
- Dead updater lock recovery retains an identity-bound tombstone, so a delayed concurrent contender cannot steal a newly acquired live lock.
- Install/update/rollback activation holds while a live Dori exists, even when idle. These commands do not kill or restart sessions. Failed verification keeps the prior source active; rollback verifies the retained previous release. The activation journal supports interrupted pointer/state reconciliation.
- Installation starts no provider, messenger, watcher, service, presence process or harness. Users supply their own AI and messenger authentication; consumer release management needs no porting models, `gh` or GitHub write permission.

### Maintainer interfaces

- Existing source-port/check/fresh-review, promotion, supervisor and recovery functionality moves to `dori upstream ... --config FILE`. There is no legacy `dori update stage/watch` alias.
- The checkout installer moves to `scripts/install-source.sh`, retaining its source-user behavior. Public consumers use the separate release bootstrap.
- `bun scripts/release.ts build --out DIST --prerelease` packages actual committed source into the fixed release assets. The root `package.json` remains the canonical version source; public archives exclude `ref/omp-ulw` and keep the exact external dependency pin.
- Tag publication requires successful exact-commit `check`, `test-macos`, `e2e-herdr` and `e2e-hc` results from the repository's trusted CI workflow and GitHub Actions app. Existing tags/releases/assets are not overwritten.

### Discord operation

Native `dori inbound discord` provides owner-only bot REST intake for one exact channel, with required bot token/owner/channel environment and a persistent per-channel cursor. Its default polling interval is three seconds; `--loop` remains minutes, with `0` for one-shot operation. A cold start records the latest message without replaying history; resumed polling emits new owner text chronologically. Startup checks authentication, channel access and Message Content Intent. The supported text flow needs no Node, external agent-messenger watcher, private helper or ad-hoc SDK adapter. Both intake and presence remain explicit native-monitor opt-ins, with the token loaded only inside each child from the user's protected file.

Discord REST polling and message sending are separate from online presence. An explicitly enabled `dori presence discord` process must remain running under the owning OMP session's native monitor supervision to keep the bot online. Readiness follows a genuine Gateway READY/RESUMED dispatch, the first heartbeat acknowledgement of each connection is logged, and reconnect/terminal errors are distinguished from readiness. Later routine acknowledgements update lifecycle state silently. Explicit stop ends the presence lifecycle and makes the bot offline; installation does not silently enable it or restore unrelated paused watchers.

### Retained source features and verification limits

The port retains messenger/lane commands, host and heavy-slot guards, claim-done evidence checks, worklog/checkpoints/lessons, the complete lead/duty harness, duty/hourly loops, inbox/backlog, runtime migration, review/merge gates, hooks and telemetry. OMP roles, live-process session identity, approval-aware delivery, native goals and recorded monitor intent remain part of the runtime integration. Slack, Telegram and hc source features remain available to maintainers.

CI coverage for macOS/hc is distinct from supported live consumer behavior. A source test, isolated runtime fixture, clean install/recovery run and actual messenger intake/reply establish different things. This changelog is not a claim that a future QA run passed, a model/provider was exercised, an independent external-user beta took place or an asset was published. Actual release QA, exact-head CI and publication readback belong with that release's evidence.

The terminal-admission integration group uses a test-local 20-second budget for two distinct real Git/fsync candidate lifecycles. Runtime command deadlines, assertion coverage and the default budget of unrelated tests are unchanged.

The Sustainable Use License, OMP attribution in `LICENSE.omp-dori`, `NOTICE`, public dependency pin and third-party notices remain unchanged.
