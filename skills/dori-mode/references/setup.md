# Setup

Run these once, in order. Stop and ask the owner wherever a step needs their choice or their hand.

## 1. herdr or hc

The Dori runs its sessions on one of two runtimes through the shared runtime abstraction. herdr gives every lane a tab, while hc (herdr-core) keeps each lane in a labelled terminal. This OMP port preserves both adapters, binds OMP session and approval records to the live agent process, and holds delivery when agent status or foreground identity is unknown.

For the first public preview, the live consumer support scope is Linux x64/herdr/Discord, with tested Bun 1.4.4 and native OMP 18.18.8. hc and the other messengers remain advanced source features with separately scoped coverage. Install from the [verified exact release](../../../docs/releases.md) and use your own provider/bot credentials. Installation starts none of the sessions or monitors below.

herdr:

```sh
curl -fsSL https://herdr.dev/install.sh | sh
herdr integration install omp
herdr --skill                       # load its skill unless it is already in your context
```

hc:

```sh
curl -fsSL https://releases.herdr.dev/core/install.sh | sh
mkdir -p ~/.config/herdr-core && echo 'notes = false' >> ~/.config/herdr-core/config.toml
hc --skill                          # load its skill unless it is already in your context
```

`notes = false` stops hc from typing notes into a parent agent on its own, so the Dori's own sends are the only text that reaches a lane. With both installed, the Dori runs on hc unless `"runtime": "herdr"` is set in step 4; with only one on PATH, it uses that one.

## 2. Messenger

For the supported Discord preview, use the shipped native Dori bot commands. You need the owner's own Discord application/bot token, not a user-account session. Node and agent-messenger are not prerequisites for this Discord text flow. The bot is created and invited through the [Discord Developer Portal](https://discord.com/developers/applications); no external watch command or private helper is part of this installation.

For other retained messenger integrations, [agent-messenger](https://github.com/agent-messenger/agent-messenger) remains optional platform-specific tooling with its own prerequisites and login procedures. Ask the owner which platform/identity to use before setting up those integrations.

- Telegram: stream a reply as it grows and show "Thinking…" with message drafts, send rich messages for tables and headings, and use topics in the private chat (turn on threaded mode in BotFather's mini app; there is no API for that switch).
- Discord: edit your own messages to grow them, and use threads.

### If the owner picks Slack: user token or bot token

Ask which mode, give the trade-off as a short list, and wait for the choice:

- **User token**: the Dori acts as a real member of the workspace.
  - It needs a paid seat, which the owner pays for.
  - It reads everything that member can see.
  - It can keep a green online dot (`dori presence slack`).
- **Bot token**: the Dori is a Slack app.
  - There's no seat cost.
  - It only sees channels it is invited to, within the scopes the app was granted.
  - It shows as an app, not a person.

Record the choice. With a user token, set `DORI_SLACK_TOKEN` and `DORI_SLACK_COOKIE` (the `d` cookie from that member's browser session). With a bot token, set `DORI_SLACK_TOKEN` to the `xoxb-` token. If you run a second Dori home on the same machine, also write that home's token to `slack-token` next to its `config.json` (`chmod 600`): `dori launch` then hands its lanes the file's path, never the token (see `dori launch` in `scripts.md`).

## 3. Bot

For Discord, create/invite the owner's own bot through the Developer Portal and finish the explicit bot setup below. Other retained messengers have their own browser/login flows. Then give the bot its avatar: the default is `assets/dori-avatar.png` in this skill, and the owner can swap in any image.

- **Telegram**: @BotFather → `/setuserpic` → pick the bot → send the image.
- **Discord**: Developer Portal → your application → **General Information** → App Icon, and **Bot** → Icon.
  - Grant **View Channel** and **Read Message History** on the chosen channel. Replies need **Send Messages**; thread replies need **Send Messages in Threads**, and creating public threads additionally needs **Create Public Threads**. Channel permission overrides still apply. See [Discord permissions](https://docs.discord.com/developers/topics/permissions).
  - Under **Bot → Privileged Gateway Intents**, enable **Message Content Intent** for ordinary owner text. Discord applies content access across APIs, including REST; an online bot or successful API call is not proof that message content is available. Complete any approval Discord requires for the application. See [Message Content Intent](https://docs.discord.com/developers/events/gateway#message-content-intent).
  - Store the token alone in an owned regular `~/.dori/discord-token` file with mode `0600`. Keep its value out of chat, source, command arguments and monitor receipts. The native monitor recipes in the release guide load that file inside the child process, rather than exporting the token into the parent OMP session.
- **Slack**:
  - bot token: the app's **Basic Information** → Display Information → App icon;
  - user token: the member's profile photo (`users.setPhoto`, or the profile page).

Greet the owner through the bot before doing anything else.

## 4. OMP extension and scripts

For public releases, follow the [exact-tag install and checksum procedure](../../../docs/releases.md#install-the-exact-preview). The explicit CLI is `~/.dori/bin/dori`; add `~/.dori/bin` to your PATH. The consumer installer checks the required existing toolchain, installs the skill/role-gated extension and exact pinned public omp-ulw, and creates config only when absent. It preserves existing configuration, authentication and work/session data. Local `--from` assets can still require the first anonymous dependency fetch.

Maintainers installing an ordinary source checkout use the separate checkout installer from that repository root:

```sh
DORI_NO_PULL=1 DORI_SRC="$PWD" bash scripts/install-source.sh
```

This is not the release bootstrap and must not be run inside a frozen consumer release. See [source installation](../../../docs/releases.md#source-checkouts-for-maintainers) for fast-forward behavior and explicit path overrides.

Edit `~/.dori/config.json`: for this preview choose `runtime: "herdr"`, `leadPane` (from `herdr pane current`), `laneWorkspace`, `defaultCwd`, a provider-qualified `defaultModel` available to your account and messenger hooks. Keep the installer's `agentCommand`, `sessionsDir` and `ompUlwPath`; `references/scripts.md` explains every field. Authenticate OMP yourself. Before starting `omp --dori-role dori` in herdr, set the non-secret `DORI_DISCORD_OWNER_ID` and `DORI_DISCORD_CHANNEL_ID` as positive decimal ID strings in that shell. The channel is the exact destination to read, including a thread only when its own ID is chosen. Keep `DORI_DISCORD_TOKEN` in the protected token file for child-only loading; no maintainer authentication is supplied.

The installed `dori` provides messenger/lane commands and consumer `dori update check|install|status|rollback`. Maintainer porting is `dori upstream ... --config FILE`. The complete upstream harness is available through `dori harness <args>` or `bun src/cli.ts`; linking the root package separately exposes `dori-harness`.

## 5. Monitors

Enable only the monitors the owner explicitly wants, using the OMP Dori extension's native `monitor` tool in the owning session. The extension records start/stop intent and restores it when that session resumes; an explicitly stopped monitor stays stopped. Its boot metadata mirror contains monitor specifications, so `dori boot` can recommend cleanup without reading private transcripts; it stops nothing itself. Installer, consumer update and rollback start none of these monitors.

| Monitor | Command | Filter |
|---|---|---|
| Discord owner intake (opt-in) | `dori inbound discord`, using the [protected-token child argv](../../../docs/releases.md#discord-owner-intake) | `^INBOUND` |
| done flow | `dori watch` | `^LANE_` |
| dead panes | `dori dead-panes --loop 3` | `^(DEAD_PANE\|UNRECOGNISED_PANE)` |
| host guard | `dori guard --loop 1` | `^HOST_GUARD` |
| freshness | `dori freshness --loop 5` | `^(NUDGED\|POSTED\|NO-REPORT\|PENDING-REPORT\|STALE-REPORT\|STALLED\|JOURNAL-ERROR\|WORKING-WHILE-PAUSED)` |
| Slack inbound (Slack only) | `dori inbound slack --loop 1` | `^INBOUND` |
| Telegram inbound (Telegram only) | `DORI_TELEGRAM_TOKEN=… DORI_TELEGRAM_OWNER_ID=… dori inbound telegram --loop 1` | `^INBOUND` |
| presence (opt-in) | `dori presence discord` (or retained Slack: `dori presence slack`) | `^PRESENCE_` |

The supported public receiver is **`dori inbound discord`**. It requires the owner's bot token and the two configured IDs, polls every three seconds by default, and emits only that owner's text from the exact channel. `--loop` is in minutes: `--loop 0.1` polls every six seconds and `--loop 0` runs once. First start saves a latest-message baseline without replay; wait for `INBOUND_READY discord owner=... channel=... bot=...`, then send a fresh owner message. Later records are `INBOUND discord <channel> <message> from=owner ref=<id|-> <JSON text>`. An owned per-channel cursor survives an explicit restart.

Use the [concrete native monitor argv recipe](../../../docs/releases.md#discord-owner-intake), which loads the protected token file only inside the child shell and inherits the two non-secret IDs from the owning OMP environment. No private helper, external CLI watcher, Node or agent-messenger is required. Startup checks bot authentication, Message Content Intent and View Channel/Read Message History access. Non-owner/bot/webhook/system messages do not become owner requests. Send permissions are required only for replies. See `scripts.md` for cursor/error details.

Discord REST inbound polling and message sending alone do not keep the bot online. While Dori is used, an explicitly enabled `dori presence discord` Gateway keeper must remain running under native monitor supervision, with `DORI_DISCORD_TOKEN` supplied only in its child environment. Genuine Gateway READY/RESUMED produces `PRESENCE_READY`; only the first actual heartbeat acknowledgement per connection produces `PRESENCE_ACK`, while later acknowledgements update lifecycle state silently. Explicit monitor stop ends its sockets/retries and prints `PRESENCE_STOPPED discord`, so offline is the intended result. The inbound watcher is a separate opt-in process. Do not restore unrelated paused watchers or the maintainer updater to fix presence. See [presence operation and errors](../../../docs/releases.md#discord-online-presence).

## 6. First look around

Once the bot is live, explore the machine: running coding-agent sessions, the tools in use, recent logs. Write what you find into memory, and adopt any running lanes with `dori adopt`.
