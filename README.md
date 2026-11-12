<p align="center">
  <img src="skills/dori-mode/assets/dori-avatar.png" alt="Dori" width="120">
</p>

# omp-dori

First public preview: Linux x64, herdr and Discord.

English | [한국어](README.ko.md) | [简体中文](README.zh.md) | [Deutsch](README.de.md) | [日本語](README.ja.md) | [Italiano](README.it.md)

[CI](https://github.com/vntrevx/omp-dori/actions/workflows/ci.yml)
[![License: SUL-1.0](https://img.shields.io/badge/license-SUL--1.0-blue.svg)](LICENSE)

Dori lives in your Slack, Discord or Telegram. You tell him what to do in a thread, he gives the job its own coding-agent session, and he writes back when the work is proven done: a merged PR, a closed issue, a published version. Between requests he finds the next task himself. That's the whole setup.

This is a modified OMP port of [`sisyphuslabs/dori-superagent`](https://github.com/sisyphuslabs/dori-superagent/tree/97d807b9037cb050a72129659cf6306d8055010d), based on upstream `97d807b9037cb050a72129659cf6306d8055010d` and the local OMP integration at `75a1b28de8caddb6d30b2e3d72ef3dec09450b47`. The Sustainable Use License and original notices remain; see [NOTICE](NOTICE).

**`v0.1.0` is a GitHub prerelease for Linux x64 / herdr / Discord.** The tested baseline is Bun **1.4.4** and native OMP **18.18.8**; these are also the minimum versions, not a claim that every newer combination has been tested. Install an exact verified release, then choose when to start Dori. Installation prepares code, dependencies and links without starting a messenger, Gateway presence keeper, watcher, service, harness or model request.

macOS and hc have CI coverage and retained source adapters; they are outside this first live consumer support scope. Slack and Telegram features remain in the source. Preview support does not imply an independent external-user beta or universal compatibility. See the [release installation and support guide](docs/releases.md), [release notes](CHANGELOG.md) and the canonical [`v0.1.0` release path](https://github.com/vntrevx/omp-dori/releases/tag/v0.1.0).

Dori mode (`skills/dori-mode`) is the bot you talk to: a skill for [OMP](https://github.com/can1357/oh-my-pi) plus the primary installed `dori` CLI that opens, tracks and closes work sessions and talks to Slack, Discord and Telegram.

The complete harness (`src/`) remains available through `dori harness <args>`, `dori-harness <args>` after an optional root package link, or `bun src/cli.ts <args>`. It provides lead/duty sessions, the duty and hourly loops, lanes, inboxes, backlog, migration, review and merge gates, hooks and telemetry on Bun without runtime npm dependencies of its own. Sessions use upstream's [herdr](https://github.com/herdrdev/herdr) or hc (herdr-core) runtime abstraction, with OMP replacing the upstream omo agent. The retained [omp-ulw](https://github.com/dbc-hbin/omp-ulw) prompt port is pinned by [`omp-ulw.lock`](omp-ulw.lock).

For maintainers, `dori upstream <check|stage|watch|status|snapshot|restore|recover|restart> --config FILE` stages Dori-only updates in independent source clones using real configurable port/check/fresh-review commands. The default is candidate-only. An explicit owner-approved `promotion` configuration additionally publishes the exact verified export through an own-repository branch, exact-head GitHub CI and fast-forward main, and can replace a configured Linux/herdr Dori with a health-checked managed release. Work, session references, intake and model policy remain in place; failed startup restores the prior execution source when safe. See [configuration, supervision and recovery](skills/dori-mode/references/scripts.md#staged-dori-updates), the [staging example](examples/update.config.json) and the [external supervisor unit](examples/update-supervisor.service). Installation never activates this policy; ULW remains pinned.

Native automated workers and reviewers require exactly one provider-qualified `--model`; `--provider`, `--models`, duplicates and implicit or bare models are rejected at configuration load. Every configured `--mode` must be `json`. The controller supplies role-specific multiline literals through OMP's `--system-prompt` and `--append-system-prompt`, so project and ancestor `SYSTEM.md`, `APPEND_SYSTEM.md` and `SYSTEM_TEMPLATE.md` files remain task data. Configured system/template/profile/alias/extension/hook/trusted-extension/Dori-role overrides are rejected before any worker or check starts, while `modelAgentDir` stays a trusted reference without credential copies. This boundary isolates paths and environment; it is not an OS sandbox.

## What you get

- You write in a thread. Dori marks the message with an "eyes" reaction, answers within a minute, and while a job runs its status is one message that he edits in place, with a short progress note every 15 minutes and at each milestone.
- Every job runs in its own herdr tab or labelled hc terminal, recorded in a registry from launch to close, as many at once as the host carries. A host guard holds new launches when memory, disk or the session count cross its line.
- No "done" without proof. A lane claims done with evidence, and the claim is checked against live state: the PR is merged, the issue is closed, the version is on npm, the command passes, the URL answers. A claim that doesn't hold goes back as not done within 5 minutes, and a lane with unpushed work can't close.
- Only the merge gets a ping. The lead is the only session that talks to you, and it reports a code change after the merge, with the PR and the merge commit. Lanes and the duty loop never talk to you.
- Work goes on while you're away. Briefs wait in a backlog. Once an hour the loop retires finished items, audits the ones nobody closed, starts the next ones that need no decision, and asks you each open question once.
- Each job gets its own thread or Telegram topic, named with its state (`[working]`, `[waiting]`, `[done]`), archived when it's done and reopened when you write in it again.
- A voice message is transcribed through your speech-to-text command and handled like text.
- Nothing is lost on a restart. The worklog of your requests, each lane's checkpoints and the registry live on disk, and `dori boot` reads them back before the Dori trusts what it remembers.
- He replies in the language of the thread, matches how you write, keeps messages short, and uses no emojis.

## Where Dori lives

Slack, Discord and Telegram are implemented in `skills/dori-mode/scripts/src/messenger/`. The table lists what each one does today; what isn't on it, the code doesn't do.

| | Slack | Discord | Telegram |
|---|---|---|---|
| Post and edit in place | Yes, and reply in threads | Yes; mentions in the text never ping anyone | Yes, plain or HTML with tables |
| Threads | Slack threads | Start, rename and archive threads | Forum topics: create, rename, close, reopen |
| Typing and streaming | | Typing indicator | Typing indicator, and a draft that grows while the answer is written, starting as "Thinking…" |
| Inbound, how your messages arrive | `dori inbound slack`: the Threads view, every thread the Dori posted in (tagged or not), and unread DMs, group DMs and mentions | `dori inbound discord`: native bot REST polling, owner-only text in one configured channel, persistent cursor | `dori inbound telegram`: a long poll that passes only the owner's messages, DMs and one group (one topic of it when `DORI_TELEGRAM_TOPIC_ID` is set), edits included, media as file ids |
| Presence | `dori presence slack` keeps a member account shown online | `dori presence discord` identifies on the gateway as online | |
| Files | Upload from the library | | File ids on inbound media |
| Tokens / intake scope | `DORI_SLACK_TOKEN`, plus `DORI_SLACK_COOKIE` for a user token | `DORI_DISCORD_TOKEN`; intake also requires `DORI_DISCORD_OWNER_ID` and `DORI_DISCORD_CHANNEL_ID` | `DORI_TELEGRAM_TOKEN`, `DORI_TELEGRAM_OWNER_ID`, and optionally `DORI_TELEGRAM_GROUP_ID` and `DORI_TELEGRAM_TOPIC_ID` |

On Slack you choose at setup how Dori appears. With a user token he is a real member: a paid seat, he sees everything that member sees, and he can hold a green online dot. With a bot token he is an app: no seat, only the channels he is invited to, within the scopes you grant.

For the supported Discord preview, create/invite your own bot through the [Discord Developer Portal](https://discord.com/developers/applications) and use the shipped native Dori commands; Node and agent-messenger are not required for this text flow. [agent-messenger](https://github.com/agent-messenger/agent-messenger) remains optional tooling for other platform workflows. The helpers in the table call each platform's API directly, retry rate limits with the server's wait time, and refuse any text containing `$(`, since message text is data and never passes through a shell. On every platform, `dori transcribe` turns a voice note into text through the command you configure as `hooks.transcribe`. There is no Teams, WhatsApp, LINE, WeChat, iMessage, KakaoTalk or Lark support in this repository, and `dori send` refuses a channel it does not know.

`dori send <slack|telegram|discord> --to TARGET --text TEXT [--thread ID] [--edit ID]` posts or edits a message from the shell. The same classes (`Slack`, `Discord`, `Telegram`) are importable modules that take their HTTP client and clock as arguments (the typing and presence helpers take timers), which is how the tests run without the network.

## Quick start

`dori` belongs to the skill CLI. Use `dori harness ...` for every harness command below: skill `dori send` posts to a messenger, while `dori harness send` delivers to an agent session; skill `dori report` and harness `dori harness report` also have different flags. Root `bun link` exposes only `dori-harness`, so it does not replace the skill command. The root CLI remains callable directly with `bun src/cli.ts`.

### Install the verified preview

Have git, [Bun](https://bun.sh) 1.4.4, [native OMP](https://github.com/can1357/oh-my-pi) 18.18.8 and [herdr](https://github.com/herdrdev/herdr) installed on Linux x64 and on `PATH`. The installer does not install or upgrade this toolchain. Download tools in this example are `curl`, `sha256sum` and `tar`. Installing or updating a release needs no `gh`, GitHub write permission or porting-model calls; AI use later needs your own provider authentication.

Check that the release belongs to **`vntrevx/omp-dori`** and is tagged **`v0.1.0`**. Download all four assets from that exact release and verify the checksums before running its installer:

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

Keep the PATH export in your shell configuration if desired. The explicit launcher is always `~/.dori/bin/dori`; no global `bun link` is needed. Checksums bind the downloaded files together; the trusted identity comes from the canonical HTTPS repository/release. The installer also checks the tag, commit/tree, archive and extracted source manifest. There is no detached-signature claim. `DORI_RELEASE=v0.1.0 bash ./install.sh` selects the same preview explicitly for a network install; a default latest request selects stable releases only and can report that none exists.

The installer creates config only when absent, preserves existing configuration/authentication/work/session data, fetches public omp-ulw at [`omp-ulw.lock`](omp-ulw.lock), and routes the CLI, skill and extension through the single `~/.dori/src` release pointer. Existing unmanaged source directories or conflicting non-link destinations are held for inspection. `--from` supplies local Dori assets; first-time pinned dependency retrieval can still need anonymous network access. See [layout, migration and troubleshooting](docs/releases.md).

### Authenticate and start, explicitly

Use your own OMP provider account and your own Discord application/bot credentials. Configure `runtime` as `"herdr"`, `leadPane`, `laneWorkspace`, `defaultCwd`, a provider-qualified `defaultModel` you can use, and messenger hooks in `~/.dori/config.json`; retain the installer's dependency paths. Create/invite the bot through the [Discord Developer Portal](https://discord.com/developers/applications), grant its channel permissions and enable Message Content Intent. Keep its token in an owned mode-0600 file and load it only in the chosen monitor children. The [setup guide](skills/dori-mode/references/setup.md) covers the native bot path; no maintainer credentials, Node or external messenger watcher are required.

Run `herdr integration install omp`. Before starting OMP inside herdr, set the non-secret decimal-string `DORI_DISCORD_OWNER_ID` and `DORI_DISCORD_CHANNEL_ID` in that shell; the latter is the exact channel or thread to receive. Then start `omp --dori-role dori` and tell it: Dori mode. Choose its name and permitted onboarding tools. Starting AI sessions can use your provider quota.

Explicitly enable `dori inbound discord` under that session's native `monitor` tool, using the [child-only token-file argv recipe](docs/releases.md#discord-owner-intake). It polls every three seconds by default; `--loop` is in minutes (`--loop 0.1` is six seconds, `--loop 0` is one-shot). On first start it records the latest message without replaying history: wait for `INBOUND_READY discord ...`, then send a fresh owner message. Subsequent owner text is emitted as `INBOUND discord ...` and the committed per-channel cursor survives restart. No private helper or external messenger watcher is needed.

Discord REST intake and message sending do not keep a bot shown online. To keep it online while Dori is in use, separately supervise `dori presence discord` with the native `monitor` tool using its [child-only token-file recipe](docs/releases.md#discord-online-presence). Keep the Gateway process running. Intentionally stopping its monitor makes the bot go offline; stopping intake alone does not change presence. Installation and updates never enable either monitor or restore an explicitly stopped watcher.

The operating contract is [`skills/dori-mode/SKILL.md`](skills/dori-mode/SKILL.md); setup, sessions, the done protocol and every command are in its [`references/`](skills/dori-mode/references/).

### Consumer updates and rollback

```sh
dori update check --version v0.1.0
dori update install --version v0.1.0
dori update status
dori update rollback
```

The API is `dori update <check|install|status|rollback> [--version TAG] [--from DIST]`. Use `--from` with `check` or `install` for a verified local asset directory. `rollback` restores the retained previous release; an optional `--version` asserts its exact tag. `status` and `rollback` do not accept `--from`. Consumers take fixed releases, never arbitrary `main`, and need no upstream port/review models or repository write access.

Plan downtime before installation, update or rollback: activation holds while a live Dori is detected, including an idle one. Explicitly stop its monitors and end its owning session using your normal runtime controls, inspect status, then retry. The release command does not kill or restart Dori. Failed verification leaves the previous source active; rollback verifies the retained previous release and switches source without rewriting user config or resuming monitors. See [update limits and recovery](docs/releases.md#updates-and-rollback).

Consumer-managed `ompUlwPath` values under `~/.dori/releases/consumer-dependencies/omp-ulw-<commit>` follow the loaded release's own lock pin across update and rollback without config writes. Explicit paths outside that cache shape remain user choices; source checkouts and the maintainer source container keep their configured paths.

### The harness

Use the same prerequisites and extension/omp-ulw installation as Dori mode. `gh` supplies PR head SHAs, checks and merges. The following commands start real sessions and loops; do not start a second harness against an existing installation's runtime and state.

Source development uses a separate checkout and [`scripts/install-source.sh`](scripts/install-source.sh); see [maintainer installation](docs/releases.md#source-checkouts-for-maintainers). Do not run source-install or dependency-editing commands inside a frozen consumer release.

Start the chosen runtime's server, then:

```sh
dori harness up
# UP runtime=herdr lead=w1:p1 duty=w1:p2 loop=w1:p3 hourly=w1:p4 home=~/.dori
```

On herdr this opens a workspace named `dori` with a lead pane (`omp --dori-role dori` with `prompts/lead.md` appended), a duty pane (also role `dori`, with `prompts/duty.md`), the duty loop below it and the hourly loop below that. hc opens four labelled terminals instead. Talk to the lead. It writes a brief (goal, inputs, deliverable, stop condition, constraints; see [`examples/brief.md`](examples/brief.md)) and opens lanes itself. You can do the same from a shell:

```sh
dori harness start fix-flaky-login --brief examples/brief.md
# STARTED fix-flaky-login pane=w1:p5 brief=~/.dori/briefs/fix-flaky-login.md
dori harness status
```

Inside a harness lane, OMP reports and finishes with the root CLI commands its lane rules supply:

```sh
dori harness report fix-flaky-login --text 'reproduced: 1 failure in 20 runs, shared session fixture'
dori harness report fix-flaky-login --status waiting --text 'WAITING: checks on owner/repo#12'
dori harness ready fix-flaky-login --text 'root cause fixed in owner/repo#12, checks green' --evidence https://github.com/owner/repo/pull/12
```

The lead, on `[READY fix-flaky-login]`:

```sh
dori harness review owner/repo#12          # fresh OMP reviewer at the head SHA; receipt; exit 0 on CLEAR
dori harness merge owner/repo#12 --dry-run # MERGE_OK ... or MERGE_REFUSED ... with exit 2
dori harness merge owner/repo#12           # gh pr merge --squash --match-head-commit <sha>
dori harness done fix-flaky-login          # closes only a READY harness lane
```

`dori harness --help` and `bun src/cli.ts --help` print the complete root command set. `dori` with no arguments prints the skill commands.

## OMP session contract

- Lead and duty use OMP role `dori`; implementation lanes and the built-in reviewer use role `lane`. The extension records `omp-dori:role` in the session branch, restores it on resume and session changes, and passes it to subagents. A plain OMP session gets no Dori event behavior, goal or monitor activation.
- Only role sessions run the pinned omp-ulw handlers. Command/tool names are registered at load time and may appear in plain sessions, but their handlers are gated. Approval event hooks are installed once a role session is used; OMP's resulting read-speculation change lasts for that process.
- The extension writes `<stateDir>/sessions/<encodeURIComponent(id)>.json`, using `HC_TERMINAL_ID` before `HERDR_PANE_ID`. Records contain `paneId`, role, session ID/file, cwd, writer pid, timestamps, goal, `approvalPending` and optional `agentStatus` (`idle`/`working`, independent of goal state). The runtimes bind them to the current live OMP process and foreground before trusting them; stale, malformed or foreign records hold delivery. A pending tool approval blocks input even with force.
- In hc, role sessions emit native OSC 7501 `app=omp`: idle at session start/agent end, working at agent start, and blocked while approvals are open. Removing an owned role record invalidates the owned report as unknown. Runtime unknown remains held, and hc's conditional `--if` delivery gates remain in force; a file record is not a substitute for a native status report.
- Goal-less lanes get OMP's native `goal` tool. Goals remain agent-owned: a brief tells the lane to create one; the port does not fabricate one. Active, paused, complete and dropped remain distinct; budget-limited and unknown goal states are held as blocked rather than treated as no goal. Skill `dori pause`/`resume` read back the native goal state and print an unverified result (exit 3) when it is not confirmed. A resumed OMP goal may be paused; inspect it and use `/goal resume` when appropriate.
- Role `dori` gets the retained `monitor` tool. Monitor argv/filter/cwd are recorded in its session branch and mirrored at `<stateDir>/monitors/<encodeURIComponent(sessionId)>.json` as `{version:1,sessionId,monitors:[{name,argv,filter,cwd}]}`. `dori boot` inspects that desired state read-only without private transcript reads. Resuming that Dori session restores recorded monitors; switching away stops its live monitors. A fresh install or container creates no receiver or watcher. Reusing a previously active session/volume can restore what that session recorded.

Skill lanes default to `anthropic/claude-opus-5-5`, with argv `["omp","--dori-role","lane","--model","{model}","{prompt}"]`. An explicit `dori launch --model` wins over `defaultModel`; configured `agentCommand`, session paths and explicit external `ompUlwPath` values remain overrides. The harness defaults to `DORI_AGENT_CMD=omp`, adds the appropriate role only when it is not already explicit, and otherwise preserves the command and its model arguments. Without a model argument it uses OMP's configured model; `review --model` remains available.

## How the harness works

![architecture](docs/architecture.png)

Retained upstream diagram; this port uses OMP and either runtime. Shell commands below include the `harness` dispatch.

Three agent roles use OMP sessions in herdr panes or hc terminals. herdr provides workspaces, tabs and panes, recognises OMP through its integration and reports agent state over its CLI; hc uses agent-reported OSC 7501 state. Dori reads that state and the process-bound extension record before delivering input.

The lead is the only session that talks to you. It turns a request into a brief, opens a lane with `dori harness start`, reads what lanes send back, and merges.

A harness lane is one `omp --dori-role lane` session, started with one brief and the lane rules appended to it (`prompts/lane-footer.md`). It reports milestones with `dori harness report` and finishes with `dori harness ready`, which requires an evidence link.

The duty loop is `dori harness duty --interval 60`, a plain process next to the duty OMP session. Each tick it flushes queued messages into sessions that have gone idle, nudges silent lanes, escalates to the lead, and marks a lane whose session vanished as lost. `dori harness duty stop --text 'why'` is its kill switch; until `dori harness duty resume`, every tick only prints why it is stopped.

The hourly loop is `dori harness loop`, started by `dori harness up` in a fourth session. Work waits in a backlog under `~/.dori/backlog/`, one brief per item, added by the lead or imported from GitHub issues that carry a label:

```sh
dori harness backlog add search-index --brief briefs/search.md --priority 2
dori harness backlog add new-billing --brief briefs/billing.md --decision 'charge per seat or per workspace?'
dori harness backlog import owner/repo --label dori # p1..p9 sets priority; needs-decision marks a question
```

A run that passes its gate (not stopped, not locked by another loop, at least an interval since the last run) first syncs items against lane and PR state. Then it audits missed work: a lane silent for 3 hours, a READY lane nobody closed for 2, a vanished pane. Then it pulls the next open items, within `--max-lanes` (default 3) open lanes and `--max-starts` (default 2) per run, while the host has load and memory to spare. Each open question reaches the lead once as `[DECIDE <id>]`, with one reminder after 6 hours, and the run ends with one `[LOOP ...]` report to the lead.

The loop only opens lanes and sends messages; it never merges, pushes or closes a pane. The full design is in [`docs/hourly-loop.md`](docs/hourly-loop.md).

![lane lifecycle](docs/lane-flow.png)

Retained upstream lifecycle diagram. Harness READY/`done` and skill claim-done/`close` are separate protocols and registries.

`dori harness start <key> --brief FILE` writes the brief plus the lane rules to `~/.dori/briefs/<key>.md`, opens a tab or terminal, runs OMP with a prompt naming the brief, records the lane as `working`, and sends the lead `[START <key>]`. From there the lane reports (`[REPORT <key>]`, or `[REPORT <key> WAITING]` when blocked outside its own session) and ends with `[READY <key>]` plus evidence links. An idle lane gets a nudge at 10 and 20 minutes and escalates at 40. A session that disappears before READY produces `[LOST <key>]`.

State is JSON on disk under `~/.dori` (or `$DORI_HOME`): `lanes/`, `inbox/`, `briefs/`, `receipts/`, plus `backlog/` and `loop/` for the hourly loop and `duty/` for the duty loop's stop file. Besides the agent sessions, the only long-running processes the harness adds are the two loops, and the only runtime dependency is Bun.

### Two session runtimes

Sessions run on herdr or hc ([herdr-core](https://releases.herdr.dev/core/install.sh)), and one home can hold both. The harness talks through `src/runtime/`; every stored lane and role remembers its opening runtime, so existing-session commands go there regardless of the selected runtime. New-session precedence is `DORI_RUNTIME`, then `runtime` in `$DORI_HOME/config.json`, then `HC_TERMINAL_ID`, then `HERDR_PANE_ID`, then hc on PATH, else herdr. With neither runtime available, the CLI prints a one-line fix. `dori harness status` prints the selected source, for example `RUNTIME hc (from config)`.

hc has no workspaces or tabs: one labelled terminal holds each session. It requires agent OSC 7501 reports; extension identity alone never promotes `unknown` to idle. Messages to unknown sessions stay queued. `dori harness migrate --to hc --all` (or `--lane <key>`, `--role duty-loop`, plus `--dry-run` to plan) captures a handoff, opens and verifies the replacement, moves the inbox and closes the old session, reporting `MIGRATED`, `SKIP`, `DEFERRED` or `MIGRATE_ROLLBACK`. OMP session-file migration uses `--resume=<path>`. See [`docs/runtime.md`](docs/runtime.md) for the verb mapping and prerequisites.

## What the harness enforces

### One voice

The lead is the only session that talks to you (`prompts/lead.md`). The lane rules send every fact meant for you to the lead (`prompts/lane-footer.md`, rule 3), and the duty session never merges and never talks to you (`prompts/duty.md`).

### READY needs evidence

`dori harness ready` requires at least one `--evidence` link and final answer text. Both are stored on the lane record. `dori harness done` requires a `ready` lane, so no harness lane closes without a recorded answer.

### Busy panes are never interrupted, and nothing is sent twice

`dori harness send` probes the session's own runtime first. `idle` or `done` can receive now; other states queue under `~/.dori/inbox/`, and the duty loop flushes one batch after the session becomes idle. A shell in front, an unknown OMP identity or an open approval receives no text.

When delivery failed after text may already have reached herdr, only Enter is pressed later, into that same agent identity. If the identity changes, the entry is held and the lead receives `[DELIVERY-AMBIGUOUS <pane>]` once. Inspect the session, then settle with `dori harness inbox ack` or `dori harness inbox resend`; do not send the message again blindly.

### Nudges at 10, 20 and 40 minutes

A lane that has not reported for 10 minutes, and whose pane is not working, gets `prompts/nudge.md` in its pane, again at 20, and at 40 the duty loop sends the lead `[ESCALATE <key>]`. A lane that reported `--status waiting`, or whose pane is `blocked`, is left alone.

### Merge needs a CLEAR receipt at the exact head and green checks

`dori harness merge` reads the PR head SHA with `gh`, then requires a `CLEAR` receipt for that exact SHA under `~/.dori/receipts/`. Only `dori harness review` writes receipts: it runs a fresh non-interactive OMP reviewer on `prompts/review.md` pinned to that SHA. Without a `VERDICT:` line, no receipt is written.

Then `gh pr checks` must return at least one check, every check must be `pass` or `skipping`, and at least one must be `pass`. An empty list is never green. A push changes the head SHA, invalidating the receipt until `dori harness review` runs again.

### Hooks for your own tooling

`$DORI_HOME/config.json` may name argv templates under `hooks`: `onReady`, `onReport` and `onLost` run in the background when those events happen (with `{key}`, `{text}` and `{evidence}` filled in per argument), and `reviewCommand` replaces the built-in reviewer as long as it prints the same `VERDICT:` line. `DORI_PROMPTS_DIR` points the harness at your own copies of the prompts.

## Dori mode in depth

### Routing

A question, a status check, a lookup or a small edit is handled in the Dori's own session. Code that lands as a PR, multi-step work, or anything long or parallel gets a lane; if an idle lane already owns that repo or topic, the work goes there. If `dori can-launch` says `HOLD` (memory, disk or pane count over the guard's line), nothing opens and you are told why and when it will start.

### Done is a claim, not a fact

`dori launch` accepts a `Done =` line only when the watcher can read every signal back itself. Signals are joined with `;`, and the lane is done when all of them pass:

| Signal | Passes when |
|---|---|
| `merged <owner/repo>#N` | the PR is merged and has a merge commit |
| `closed <owner/repo>#N` | the issue is closed |
| `published <pkg>@<version>` | npm has that exact version |
| `command ["argv","as","json"] [stdout~"regex"]` | the command exits 0, and stdout matches the regex if one is given |
| `file <path> [sha256=<hex>] [json:.a.b=<value>]` | the file exists, its sha256 matches, and the JSON field equals the value |
| `url <http(s) url> [status=200] [body~"regex"]` | a GET returns that status, and the body matches if a regex is given |

A lane claims with `dori claim-done <key> --evidence "..."`. `dori watch` settles claims older than 5 minutes: unpushed or uncommitted work and failing signals turn into objections, and otherwise the lane is closed: its thread is marked done when a thread hook is configured, its tab is closed and its worktrees are removed.

### Before a PR is called done

`dori verify-pr <owner/repo> <num>` reads the PR through `gh api` and checks that every commit's author and committer email is the identity configured for that repo, and that `fix:` commits touching source come with a changed test or a `No test:` reason.

### Lessons

`dori lesson add` records a rule a lane learned about a repo, tied to the evidence that taught it. A `--global` lesson is a standing rule from the lead for every lane. `dori launch` puts the current ones above the next brief.

### Supervision

Each `freshness` tick checks launched lanes against optional ceilings on wall minutes, tokens and cost (from a usage command you configure), and marks a lane stalled when its `Working` timer keeps growing for 20 minutes with no new report, checkpoint or file change. A lane at a ceiling is never killed; it waits for the lead.

### When you say wait

`dori pause <key>` pauses the skill lane's native OMP goal before stopping its running turn, and prints `PAUSE_UNVERIFIED` (exit 3) if confirmation fails. Approval-held sessions receive no input. `dori resume <key>` reads back resume and restores the earlier lane status and quiet clock; an unconfirmed resume remains paused.

## Same structure, same philosophy

DeepSeek published [dsh-superagent-prototype](https://github.com/deepseek-ai/dsh-superagent-prototype) (first commit 2026-10-31) and, about a day later, the article "Exploring Super-agent Design: Towards a More Autonomous Agent System". Put its docs next to Dori and the concepts line up one for one, in the pipeline and in the rules of their [philosophy doc](https://github.com/deepseek-ai/dsh-superagent-prototype/blob/main/docs/000-philosophy.md).

![Dori vs DeepSeek](docs/dori-vs-deepseek.en.png)

| DeepSeek dsh-superagent | Dori |
|---|---|
| Source: GitHub, Feishu | Dori lives in chat: Slack, Discord, Telegram |
| Trigger, then Eval, then Ticket | One task asked in chat becomes one lane with one brief |
| Executor session per ticket: one session, one work directory | One coding-agent session per task, in its own herdr tab, many in parallel |
| Channel: answers in the original conversation, with a PR for example | Reports in the same thread, one status message per task, edited in place |
| Panel: shows only what needs a person | The lead asks you only for decisions that are irreversible or change the product |
| Least human attention, most value | Short messages, no emojis, a ping only after the merge |
| Acts only when asked or when a written team rule allows it | Dori starts no work nobody asked for; the backlog holds what you queued |
| Speaks only when a person must act, one question at a time | A decision comes as numbered options with his own pick |
| Reports honestly, never works around a block | No "done" without proof, and a block is reported as a block |

We have been running this shape for about six months, in public:

- 2026-04-18: team mode lands in omo, several agent sessions in terminal panes with a worktree each ([f1268c0](https://github.com/code-yeongyu/oh-my-openagent/commit/f1268c0), [e303fee](https://github.com/code-yeongyu/oh-my-openagent/commit/e303fee)).
- 2026-05-06: the Dori name enters the omo README ([#3807](https://github.com/code-yeongyu/oh-my-openagent/pull/3807)).
- 2026-10-07: this repository publishes Dori mode, as `omo-dori-mode-experimental` ([ce53e4d](https://github.com/sisyphuslabs/dori-superagent/commit/ce53e4d)).
- 2026-10-31: DeepSeek's prototype gets its first commit.

These are upstream history and usage figures, retained for attribution, not measurements of this port. Between 2026-07-13 and 2026-11-08, [code-yeongyu/oh-my-openagent](https://github.com/code-yeongyu/oh-my-openagent) merged 2,846 pull requests and landed 18,780 commits across all contributors using the full upstream setup. This port retains the published minimal harness and replaces its agent boundary with OMP.

## Configuration

Harness environment:

| Variable | Default | Meaning |
|---|---|---|
| `DORI_HOME` | `~/.dori` | state directory |
| `DORI_AGENT_CMD` | `omp` | harness agent command; OMP gets role `dori` for lead/duty and `lane` for lanes/review unless a role is explicit; command/model overrides remain intact |
| `DORI_HERDR_SESSION` | | a named herdr session |
| `DORI_RUNTIME` | enclosing hc/herdr runtime, else hc on PATH, else herdr | selected runtime for new sessions; `runtime` in `$DORI_HOME/config.json` is the next precedence; missing binaries produce a one-line fix |
| `DORI_CMD` | full path to `src/cli.ts`, run with Bun and `DORI_HOME` | how harness agents call their root CLI without depending on the primary `dori` bin |
| `DORI_PROMPTS_DIR` | the repo's `prompts/` | your own prompt files |
| `DORI_MAX_LANES`, `DORI_MAX_STARTS`, `DORI_MAX_LOAD` | 3, 2, core count | hourly loop budget |
| `DORI_TELEMETRY`, `DORI_TELEMETRY_ENDPOINT` | on, `https://t.omo.dev` | `0` turns telemetry off |

Dori mode reads `~/.dori/config.json` (or `DORI_CONFIG`); every field is optional and [`config.example.json`](skills/dori-mode/references/config.example.json) shows them. Set `runtime`, `leadPane`, `laneWorkspace`, `defaultCwd`, `agentCommand`, `defaultModel`, `ompUlwPath`, `repoIdentities` (Git identity for `verify-pr`), `hooks.threadReply`/`threadDone` (argv templates with `{thread}` and `{text}`), `hooks.transcribe`, and `guard` (load, free memory, free disk and session count). `DORI_STATE_DIR` overrides `stateDir`; `DORI_LEAD_PANE` overrides `leadPane`. OMP sessions default to `~/.omp/agent/sessions`; Linux process/session discovery remains supported. Timing defaults remain 15 quiet minutes to nudge, 20 to post the last report, and 5 to settle a claim.

Harness records live under `DORI_HOME` (default `~/.dori`); skill registry, reports, checkpoints, worklog, lessons, session records and monitor mirrors live under `stateDir` (default `~/.dori/state`). They are distinct registries sharing runtime adapters, not interchangeable lane records. When isolating a candidate, set `HOME`, `DORI_HOME`, `DORI_CONFIG` and `DORI_STATE_DIR` together; keep OMP sessions, runtime namespace and messenger fixtures separate from production.

## Run it in a container

[`examples/container/`](examples/container/) provides a maintainer source-container recipe with gh, Node, Bun 1.4.4, herdr or hc, agent-messenger, OMP 18.18.8, the extension and omp-ulw at the lock revision. The build context is this checkout; it never clones an old Dori release. Binary downloads retain pinned sha256 checks, npm packages have exact top-level versions, and source package installs use their locks. Public consumers use the verified release installer above; this recipe is not a published or independently validated consumer image. Named volumes keep isolated state and interactive logins; startup runs only the terminal runtime server and an idle command. See [`docs/container.md`](docs/container.md) for inputs, commands and acceptance steps.

## Telemetry

dori sends anonymous usage counts the same way omo does: OTLP/HTTP JSON logs to the omo collector at `https://t.omo.dev`. The identity is a random install id kept in `~/.dori/telemetry.json`, and only its sha256 is sent. On the first send the collector mints a per-install ingest key from that hash; no shared key ships in the code. The first run prints a one-line notice. Turn it off with `DORI_TELEMETRY=0` (`DO_NOT_TRACK=1` also works).

| Event | When | Fields |
|---|---|---|
| `dori.installed` | first run | none |
| `dori.daily_active` | once per UTC day, on any dori command | `day_utc` |
| `dori.lane_started` | `dori harness start` | `open_lanes` |
| `dori.lane_ready` | `dori harness ready` | `evidence_count`, `lane_minutes` |
| `dori.review_completed` | `dori harness review` | `verdict`: `CLEAR` or `BLOCK` |
| `dori.merge_decided` | `dori harness merge` without `--dry-run` | `outcome` |
| `dori.duty_summary` | at most once an hour, from the duty loop | `ticks`, `flushed`, `nudges`, `escalations`, `lost` |

Every record also carries the service name and version, the install hash, a CI flag, the OS type and version, and the CPU architecture. Never sent: paths, repo names, PR numbers, lane keys, briefs, messages or any other text. A command waits at most 2 seconds for the send, and a telemetry failure never fails a command.

## Tests and development

Use [source-checkout installation](docs/releases.md#source-checkouts-for-maintainers) for development. Maintainers port with `dori upstream ... --config FILE`, then [build and publish fixed releases](docs/releases.md#building-and-publishing-releases); consumers use `dori update`.

```sh
bun install --frozen-lockfile
(cd skills/dori-mode/scripts && bun install --frozen-lockfile)
(cd extension && bun install --frozen-lockfile)
bun run ci # Biome, tsc, knip, LOC gate, harness, skill and extension suites
```

Check results against that commit's CI and execution evidence. Unit suites use fake runtimes, gh and clocks; OMP regressions cover live-process identity, approvals, session mapping and native goal pause/resume. Real herdr/hc end-to-end tests use isolated runtime namespaces and fixture agents, and skip when their binary is absent. They do not prove model-provider behavior or messenger intake/reply; those need separate isolated QA. See [CONTRIBUTING.md](CONTRIBUTING.md) for upstream contributor conventions.

## What this is not

The retained harness is upstream's minimal published stack: it omits the cross-machine mesh, harness heavy-slot scheduling, harness chat/ticket bridges and whole-suite receipts. All published modules remain, including roles, lanes, inbox, duty/hourly loops, backlog, migration, hooks, telemetry and review/merge gates. The skill separately provides messenger helpers, host guards, heavy slots, worklog, checkpoints and lessons; both use the shared runtime abstraction. The Dori updater adds explicitly configured automation, not implicit production activation or ULW updates. Its isolated paths and environment are not an OS sandbox; configure only trusted upstream, verification and publication capabilities.

## Contributing

Upstream sisyphuslabs/dori-superagent requires every contributor to sign the [Contributor License Agreement](CLA.md) once, by commenting `I have read the CLA Document and I hereby sign the CLA` on their first pull request; the CLA Assistant check records it there. This fork's CLA workflow is disabled, so no CLA signature is required for pull requests to this fork. The gates, the test commands and the pull request rules are in [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Current upstream code remains under the [Sustainable Use License](LICENSE) version 1.0, with third-party terms intact. Lines 26-29 permit your own internal business use and non-commercial or personal use, and constrain distribution and notices; read those terms for uses beyond that grant. [NOTICE](NOTICE) identifies this modified copy. [LICENSE.omp-dori](LICENSE.omp-dori) preserves the pre-existing OMP material's notice; it does not relabel current upstream code as MIT. Upstream commits before `ba96abf` shipped under MIT. omp-ulw is fetched separately at its pinned revision and retains its own notices.
