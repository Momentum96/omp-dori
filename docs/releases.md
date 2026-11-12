# Public releases and support

## Preview support boundary

The first public release, `v0.1.0`, is a GitHub **prerelease** for **Linux x64, herdr and Discord**. The project retains the full messenger skill, harness and OMP extension; retained features and CI jobs do not expand this preview's live consumer support scope.

| Component | Minimum requirement | Tested preview baseline |
|---|---|---|
| OS / architecture | Linux x64 | Ubuntu 24.04 x64, clean non-root consumer home |
| Bun | 1.4.4 | 1.4.4 |
| Native OMP | 18.18.8 | 18.18.8 |
| Session runtime | herdr, with its OMP integration | herdr 0.9.5 |
| Messenger | Discord, using your own bot | Discord |

Newer Bun or OMP versions meeting the minimum are not automatically tested combinations. macOS and hc have CI coverage and retained adapters, rather than a first-release live consumer support promise. Slack and Telegram remain source features. The maintainer source-container recipe is separate from the public release installer; it is not a published consumer image.

Linux x64 identifies the platform and architecture, not a tested matrix of Linux distributions. Qualify the specific distribution and toolchain against the release's actual clean-consumer evidence; other Linux distributions remain compatibility targets with unverified consumer behavior.

Preview support does not imply an independent external-user beta, exhaustive provider testing or universal compatibility. Exact-head CI, isolated install/recovery checks, native runtime behavior and real messenger intake/reply are distinct evidence. Check the evidence attached to the particular release before treating any of those as exercised. This guide describes the contract and operating procedure; it is not a test-run receipt.

## Prerequisites and trust

Install git, Bun, native OMP and herdr yourself and put their binaries on `PATH`. The release installer checks prerequisites; it does not install or upgrade your toolchain. The download example also uses Bash, curl, tar and `sha256sum`. [Bun](https://bun.sh), [OMP](https://github.com/can1357/oh-my-pi) and [herdr](https://github.com/herdrdev/herdr) document their own installation and authentication.

Consumers need neither the GitHub CLI nor GitHub write permission to install, check, update or roll back Dori. These operations do not call porting/review models or change global provider settings. Later AI sessions use your own configured provider and quota; Discord uses your own application/bot account and token. No maintainer credentials, private session history or model authentication ship in the assets.

Use the canonical repository **https://github.com/vntrevx/omp-dori** and the exact tag **`v0.1.0`**. Its intended canonical release path is **https://github.com/vntrevx/omp-dori/releases/tag/v0.1.0**. Check the repository owner, tag and release commit there before downloading. Do not substitute a fork, an arbitrary `main` script or an unverified mirror.

Each release has four fixed assets:

| Asset | Purpose |
|---|---|
| `omp-dori-v0.1.0.tar.gz` | Public source archive, rooted at `omp-dori-0.1.0/` |
| `release-manifest.json` | Product/version/tag, commit/tree, archive digest/size, public source manifest, requirements and exact public omp-ulw dependency |
| `SHA256SUMS` | SHA-256 checksums for the release payload files |
| `install.sh` | Release bootstrap, separate from the source-checkout installer |

Checksums establish consistency with the files downloaded from that trusted release. They are not an independent signature or a substitute for verifying the publishing repository. The installer also validates the manifest bindings, archive digest and extracted source manifest. Tar entries are checked before extraction; links, special files and unsafe paths are not admitted. The public source manifest excludes `ref/omp-ulw`; the separately fetched dependency must match `omp-ulw.lock` exactly.

## Install the exact preview

Use an account with its own home and no existing running Dori. First inspect an existing installation rather than installing over its source or state.

```sh
download_dir="$(mktemp -d)" || exit 1
cd "$download_dir" || exit 1
release_url="https://github.com/vntrevx/omp-dori/releases/download/v0.1.0"
for asset in install.sh release-manifest.json SHA256SUMS omp-dori-v0.1.0.tar.gz; do
  curl --proto '=https' --tlsv1.2 -fL "$release_url/$asset" -o "$asset" || exit 1
done
sha256sum --check SHA256SUMS || exit 1
bash ./install.sh --version v0.1.0 --from "$download_dir" || exit 1
"$HOME/.dori/bin/dori" --version
export PATH="$HOME/.dori/bin:$PATH"
```

Only run the installer after every checksum passes. Preserve the PATH export in your shell configuration if desired; `~/.dori/bin/dori` is the explicit invocation and does not depend on global `bun link` registration. With that checked installer, `bash ./install.sh --version v0.1.0` or `DORI_RELEASE=v0.1.0 bash ./install.sh` fetches the exact network release instead. A request without an explicit version selects a stable release only; it does not silently opt into this preview.

`--from DIRECTORY` is a supported local release-asset input for the bootstrap and for consumer `check`/`install`. Copy the four matching assets together. It is **not** a completely air-gapped first-install promise: git may still need anonymous network access to obtain the exact public omp-ulw commit. A previously provisioned, owned dependency is reused only after validation. There is no separate opaque dependency bundle format.

## Installation layout and preserved data

The consumer installation uses:

- `~/.dori/bin/dori`: the stable launcher.
- `~/.dori/src`: the single active source pointer.
- `~/.dori/releases/releases/<commit>-<public-source-hash-prefix>`: verified retained release directories; the hash prefix has 16 hexadecimal characters.
- `~/.dori/releases/consumer-dependencies/omp-ulw-<commit>`: owned dependency caches for each retained release's public pin.
- `~/.agents/skills/dori-mode` and `~/.omp/agent/extensions/omp-dori`: stable references through the active source pointer.
- `~/.dori/config.json`: an example is created only if this file is absent.

The consumer keeps its own installed-state and activation journal under the release root, separately from maintainer managed-deployment journals. It stages and verifies a candidate before switching the source pointer and retains the previous source for rollback. Treat a release directory as frozen: edit your configuration or use a separate source checkout rather than changing installed product files.

Existing config, authentication, work records, sessions, history and explicit model choices are preserved. The installer does not copy another user's authentication or replace global model settings.

For code loaded from a retained consumer release, an `ompUlwPath` naming exactly `~/.dori/releases/consumer-dependencies/omp-ulw-<40-hex-commit>` (optionally followed by `/index.ts`) uses that loaded source's own `omp-ulw.lock` pin. Update and rollback therefore select the matching dependency without rewriting config bytes; already loaded code stays bound to its own release if the active pointer moves. Paths outside this managed cache shape remain explicit user choices. Ordinary source checkouts and the maintainer source-container layout keep their configured paths unchanged.

An existing real directory at `~/.dori/src`, an unmanaged pointer, or a conflicting non-link skill/extension/launcher destination holds installation rather than overwriting it. Keep the old installation and its data intact. Continue its source-checkout workflow or plan an explicit stopped migration with backups; the first preview does not silently convert arbitrary legacy layouts. Do not delete an existing installation to bypass a hold.

## Your authentication and explicit startup

Configure `~/.dori/config.json` for your own environment: `runtime: "herdr"`, `leadPane`, `laneWorkspace`, `defaultCwd`, a provider-qualified `defaultModel` available to your account, and messenger hooks. Authenticate OMP with your own provider account. Create/invite your own bot through the [Discord Developer Portal](https://discord.com/developers/applications), granting the chosen channel's View Channel/Read Message History permissions and the send/thread permissions you intend to use. Enable **Message Content Intent** on the Bot page for ordinary owner text. See the [setup reference](../skills/dori-mode/references/setup.md) and [Discord's content-intent requirements](https://docs.discord.com/developers/events/gateway#message-content-intent). Native Dori handles this supported bot text flow; Node, agent-messenger, a user-account token and an external watch script are not prerequisites.

Keep your bot token alone in an owned regular `~/.dori/discord-token` file with mode `0600`. Load it as `DORI_DISCORD_TOKEN` only within the opted-in native monitor child environment. Do not put its value in chat, command arguments, source, monitor specifications, release assets or support reports, and do not export it into the parent OMP session.

Install herdr's OMP integration explicitly:

```sh
herdr integration install omp
```

Before starting OMP inside herdr, enable Discord's Developer Mode to copy your own user ID and the exact channel ID (or the thread's own ID). Keep these as positive decimal strings. They are non-secret and can be inherited by the native monitor children:

```sh
read -r -p "Your Discord user ID: " DORI_DISCORD_OWNER_ID
read -r -p "Discord channel or thread ID: " DORI_DISCORD_CHANNEL_ID
export DORI_DISCORD_OWNER_ID DORI_DISCORD_CHANNEL_ID
omp --dori-role dori
```

Then say "Dori mode" and complete the name, messenger destination and permitted-tool onboarding. This is an explicit AI session start and can use provider quota. The harness is an additional opt-in surface: `dori harness up` starts real lead/duty sessions and loops. Do not start a duplicate against the same runtime and state.

Installation, update, rollback and `dori boot` do not start a provider, inbound watcher, Gateway presence process, systemd service or harness. Enable only the monitors you intend to operate. Resuming a previously active OMP session can restore that session's recorded monitor intent; a fresh installation creates none, and an explicitly stopped monitor must remain stopped.

### Discord owner intake

The shipped receiver is **`dori inbound discord [--loop MINUTES]`**. Required child environment is `DORI_DISCORD_TOKEN` plus `DORI_DISCORD_OWNER_ID` and `DORI_DISCORD_CHANNEL_ID`. Only the configured owner's text in that exact channel is admitted. Bots, the bot itself, other users, webhooks and system messages are excluded. For a thread, configure that thread's channel ID; the receiver does not discover all guild channels or child threads.

With the protected token file prepared above and the non-secret IDs inherited from the owning OMP session, explicitly start one native `monitor` using these `argv` values and filter `^INBOUND`:

```json
["bash", "-c", "set -eu; DORI_DISCORD_TOKEN=\"$(cat \"$HOME/.dori/discord-token\")\"; export DORI_DISCORD_TOKEN; exec \"$HOME/.dori/bin/dori\" inbound discord"]
```

The token value never appears in this argv or its monitor receipt: only the child shell reads and exports it. No private helper, external watch script, Node or agent-messenger is needed. A different token-file path is an explicit local choice; keep it owned, regular and mode `0600`.

By default the command polls continuously every **three seconds**. `--loop` uses **minutes**: `--loop 0.1` means six seconds, and `--loop 0` runs once. Append those flags after `inbound discord` in the opted-in argv when needed.

Startup validates bot authentication, application Message Content Intent and View Channel/Read Message History access before `INBOUND_READY discord owner=<id> channel=<id> bot=<id>`. On first start it atomically records the latest message ID without replaying old conversation; an empty channel starts at `0`. **Wait for readiness, then send a fresh owner message.** Subsequent owner messages appear as `INBOUND discord <channel> <message> from=owner ref=<id|-> <JSON text>` for native monitor delivery to the owning OMP session.

The cursor is committed under `<stateDir>/discord-inbound/<channelId>.json` in a private owned directory with mode-0600 atomic cursor files. Later polls page back to the cursor, deduplicate and emit owner text chronologically, advancing state as items are handled. Explicitly restarting the monitor resumes its committed cursor. Do not delete the cursor to simulate successful intake. This is new-message text polling, not edit/delete/media event capture.

Startup auth, permission, intent and state errors exit nonzero with sanitized output. Later poll failures produce `WARN` and retry on the polling loop. Inspect those errors and actual fresh owner delivery before claiming setup works; readiness alone is not an intake/reply receipt. SIGINT/SIGTERM stops REST intake. It does not set online presence, and stopping it does not stop the separately enabled Gateway presence monitor.

### Discord online presence

Discord REST inbound polling and message sending alone do not keep a bot online. For an online indicator while Dori is in use, explicitly run **`dori presence discord`** under the owning OMP session's native **`monitor`** supervision. Supply `DORI_DISCORD_TOKEN` only in that child's environment. Keep the presence command running alongside the separately enabled native `dori inbound discord` monitor; neither replaces the other.

For an explicitly enabled presence monitor, use these native monitor `argv` values and filter `^PRESENCE_`. The protected token file is read inside the child shell; the monitor specification contains only its path, never the token:

```json
["bash", "-c", "set -eu; DORI_DISCORD_TOKEN=\"$(cat \"$HOME/.dori/discord-token\")\"; export DORI_DISCORD_TOKEN; exec \"$HOME/.dori/bin/dori\" presence discord"]
```

Presence logs distinguish actual Gateway state:

- `PRESENCE_READY discord event=READY` or `event=RESUMED` follows that genuine Gateway dispatch, not merely opening a socket.
- `PRESENCE_ACK discord seq=<number|->` records only the first actual heartbeat acknowledgement of each connection; later routine acknowledgements update lifecycle state silently.
- `PRESENCE_RECONNECT discord reason=... code=... retry_in=...s` describes reconnecting, not readiness.
- `PRESENCE_STOPPED discord` records an explicit SIGINT/SIGTERM stop; sockets, timers and retries are stopped and the bot goes offline.

An authentication, configuration or intent failure exits nonzero with a sanitized Gateway error. Inspect that error and your bot setup; do not treat successful REST sends as evidence of presence. Intentionally stopping the native presence monitor means the bot should go offline. Restart that monitor only when you explicitly want presence again. Do not restore unrelated paused watchers or the maintainer updater to fix an offline indicator.

## Updates and rollback

The consumer API runs before normal Dori config loading:

```text
dori update <check|install|status|rollback> [--version TAG] [--from DIST]
```

| Command | Behavior and options |
|---|---|
| `dori update check --version v0.1.0` | Inspect an exact release without activating it; `check` also accepts `--from DIST` |
| `dori update install --version v0.1.0` | Verify and install the selected release; `install` also accepts `--from DIST` |
| `dori update status` | Inspect installed state and reconcile an interrupted activation journal |
| `dori update rollback` | Verify and restore the retained previous release |
| `dori update rollback --version v0.1.0` | Assert that the retained rollback target is exactly this tag |

`status` and `rollback` do not accept `--from`. Rollback is a retained-source operation; use `install --from DIST` to select a local candidate. Network `check`/`install` without `--version` selects stable latest only. No consumer command ports upstream, runs review models, fetches arbitrary `main`, publishes GitHub changes or grants repository write access.

**Activation holds while a live Dori is detected, even when it is idle.** Plan a maintenance window. Explicitly stop the monitors you own, end the owning Dori session using normal runtime controls, inspect `dori update status`, then retry the install or rollback. The installer and release commands never kill or restart Dori, consume an approval, alter a paused monitor or pretend a busy hold succeeded.

Failed download, integrity verification or preparation leaves the previously active source and user data in place. Rollback validates the previous release identity and source before switching; it does not undo external work, rewrite config or resume providers/monitors. There must be a valid retained previous release to roll back to. Following an interruption, the next `status`, `install` or `rollback` reconciles the pointer/state journal before continuing. Preserve those records for diagnosis instead of manually replacing pointers or deleting journals.

## Troubleshooting and support reports

- **No stable latest release:** select the preview explicitly with `--version v0.1.0`. A prerelease is not an automatic latest fallback.
- **Missing/wrong toolchain:** compare `bun --version`, `omp --version` and `git --version` with the baseline, and check that herdr is on `PATH`. Correct your toolchain yourself; the bootstrap does not replace global tools.
- **Checksum/manifest/archive failure:** stop using that download. Retain the error, confirm the canonical owner/tag, and download the complete matching asset set again. Do not disable checks or combine files from different releases.
- **Live actor, unmanaged source or path collision:** keep the existing source/state intact and resolve the reported ownership or session condition. A hold is a nonzero result, not an installed update.
- **Interrupted activation:** run `~/.dori/bin/dori update status` and retain the release state/journal if it reports an unresolved inconsistency. Do not hand-edit the active pointer to manufacture success.
- **Discord messages work but the bot is offline:** inspect the opt-in Gateway presence monitor, its genuine READY/RESUMED and heartbeat logs, your token/intents and the actual Discord indicator. REST success alone proves none of these.

For a support report, include the exact tag, release commit/source hash shown by the manifest or status, OS/architecture, Bun/OMP versions, selected runtime, command and sanitized output. State whether the input was a canonical network release or `--from`. Report checksum failure, skipped checks and unexercised behavior honestly. Exclude tokens, auth files, private session transcripts and project data. Use the repository's [issues](https://github.com/vntrevx/omp-dori/issues) after reviewing what you are submitting.

## Source checkouts for maintainers

Source installation is separate from public release installation. Use an ordinary checkout outside the frozen consumer release directories:

```sh
git clone https://github.com/vntrevx/omp-dori.git "$HOME/src/omp-dori"
cd "$HOME/src/omp-dori"
DORI_NO_PULL=1 DORI_SRC="$PWD" bash scripts/install-source.sh
```

The former checkout installer lives at **`scripts/install-source.sh`**. It retains fast-forward-only checkout updates when pulling is enabled, frozen root/skill/extension dependency installation, skill/extension links and example-config creation only when absent. `DORI_NO_PULL=1` installs the selected checkout as it is. `DORI_SRC`, `SKILLS_DIR` and `OMP_EXTENSIONS_DIR` select explicit source/link destinations; `OMP_ULW_SRC` selects an existing dependency checkout at the required pin. Keep the full sibling source layout rather than copying the skill alone. Root `bun link` is optional and exposes only `dori-harness`; it is not the consumer launcher installation method.

Use a separate owned home/state/runtime namespace for experiments. The [container recipe](container.md) is an advanced source-development example, not proof of live consumer support on every runtime or architecture. [CONTRIBUTING.md](../CONTRIBUTING.md) records source checks and contributor conventions.

Maintainer upstream porting uses:

```text
dori upstream <check|stage|watch|status|snapshot|restore|recover|restart> --config FILE
```

This is the former porting updater, with no legacy `dori update stage/watch` alias. Its worker/reviewer models, checks, promotion policy, recovery and external supervisor remain documented in the [staged-update reference](../skills/dori-mode/references/scripts.md#staged-dori-updates). Maintainers supply their own model/provider authentication and, only when publishing, repository permissions. A ready source candidate is not a consumer release or an automatic live deployment.

## Building and publishing releases

The root `package.json.version` is canonical. From the actual committed release source, build the first preview with:

```sh
bun scripts/release.ts build --out ./dist --prerelease
```

The general interface is `bun scripts/release.ts build --out DIST [--prerelease]`. The producer packages actual committed HEAD, rather than uncommitted worktree bytes or an arbitrary private source snapshot. It preserves executable bits, excludes `ref/omp-ulw`, binds the public source manifest to the commit/tree and writes the four fixed assets listed above. For another version, archive and root names are derived as `omp-dori-v<version>.tar.gz` and `omp-dori-<version>/`. A public source hash describes `ulwPresent:false`; the exact public dependency pin remains separate.

Before publication, maintainers perform isolated real-binary install/update/failure-recovery QA without personal HOME/auth/history mounts, review the change freshly and obtain the required CI evidence for the exact release commit. Report the actual results separately: source checks or a historical port replay are not a future clean-consumer run, a provider test or an independent external-user beta.

The `.github/workflows/release.yml` tag workflow validates the tag against the canonical package version. It requires successful **`check`**, **`test-macos`**, **`e2e-herdr`** and **`e2e-hc`** results for that exact commit from the repository's own CI workflow and the trusted GitHub Actions app **15368**. A tag push alone does not prove `ci.yml` ran. The first release is explicitly marked prerelease. Existing tags, releases and assets must not be overwritten; publication needs maintainer permissions and consumers need none.

After publishing, read back the actual release metadata/assets, checksums and tag/commit identity before claiming publication complete. Add the actual QA/CI evidence to the release notes. This procedure makes no claim that an unrun check or an unpublished asset already exists. [LICENSE](../LICENSE), [LICENSE.omp-dori](../LICENSE.omp-dori), [NOTICE](../NOTICE) and third-party pin/notices remain in force; the source-available upstream license has not been replaced.
