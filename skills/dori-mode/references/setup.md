# Setup

Run these once, in order. Stop and ask the owner wherever a step needs their choice or their hand.

## 1. herdr or hc

The Dori runs its sessions on one of two runtimes through the shared runtime abstraction. herdr gives every lane a tab, while hc (herdr-core) keeps each lane in a labelled terminal. This OMP port preserves both adapters, binds OMP session and approval records to the live agent process, and holds delivery when agent status or foreground identity is unknown.

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

Install agent-messenger (github.com/agent-messenger/agent-messenger) and read which platforms it supports. Ask the owner which one to use and wait for the answer.

agent-messenger wraps part of each platform. Read the platform's current bot API changelog and call the API directly for the rest:

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

Finish every login in the browser. Create the bot with agent-messenger and finish its setup. Then give it its avatar: the default is `assets/dori-avatar.png` in this skill, and the owner can swap in any image.

- **Telegram**: @BotFather → `/setuserpic` → pick the bot → send the image.
- **Discord**: Developer Portal → your application → **General Information** → App Icon, and **Bot** → Icon.
- **Slack**:
  - bot token: the app's **Basic Information** → Display Information → App icon;
  - user token: the member's profile photo (`users.setPhoto`, or the profile page).

Greet the owner through the bot before doing anything else.

## 4. OMP extension and scripts

From the repository root, use the OMP installer:

```sh
bash ./install.sh
```

It checks Bun 1.4.4+ and OMP 18.18.8+, installs the skill CLI and role-gated extension, preserves the pinned omp-ulw revision, and creates an example config only when none exists. Existing non-link directories are preserved. `DORI_NO_PULL=1` installs an existing checkout as it is; `OMP_ULW_SRC` selects an existing omp-ulw checkout and updates `ompUlwPath`.

Edit `~/.dori/config.json`: at least `runtime` (`"herdr"` or `"hc"`), `leadPane` (the current runtime's session id), `laneWorkspace` (a herdr workspace or hc group label), `defaultCwd` and messenger hooks. Keep OMP's `agentCommand`, `sessionsDir` and `ompUlwPath`; `references/scripts.md` explains every field. Then start `omp --dori-role dori` in the selected runtime. The installer starts no messenger, monitor or harness.

The installed `dori` command remains the messenger/lane CLI. The complete upstream harness is available through `dori harness <args>` or `bun src/cli.ts`; linking the root package separately exposes `dori-harness`.

## 5. Monitors

Arm these as persistent monitors with the OMP Dori extension's `monitor` tool in the owning session. The extension records start/stop intent and restores it when that session resumes. Its boot metadata mirror contains monitor specifications, so `dori boot` can recommend cleanup without reading private transcripts; it stops nothing itself.

| Monitor | Command | Filter |
|---|---|---|
| inbound messages | your messenger's watch command | new owner messages |
| done flow | `dori watch` | `^LANE_` |
| dead panes | `dori dead-panes --loop 3` | `^(DEAD_PANE\|UNRECOGNISED_PANE)` |
| host guard | `dori guard --loop 1` | `^HOST_GUARD` |
| freshness | `dori freshness --loop 5` | `^(NUDGED\|POSTED\|NO-REPORT\|PENDING-REPORT\|STALE-REPORT\|STALLED\|JOURNAL-ERROR\|WORKING-WHILE-PAUSED)` |
| Slack inbound (Slack only) | `dori inbound slack --loop 1` | `^INBOUND` |
| Telegram inbound (Telegram only) | `DORI_TELEGRAM_TOKEN=… DORI_TELEGRAM_OWNER_ID=… dori inbound telegram --loop 1` | `^INBOUND` |
| presence (optional) | `dori presence slack` or `dori presence discord` | `^PRESENCE_READY` |

## 6. First look around

Once the bot is live, explore the machine: running coding-agent sessions, the tools in use, recent logs. Write what you find into memory, and adopt any running lanes with `dori adopt`.
