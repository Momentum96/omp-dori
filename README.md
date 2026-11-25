# Cataly

Cataly is one Rust hub that runs on your machine over OMP (the coding agent).
You talk to it from a messenger (Discord, Telegram or Slack): your messages go to a long-running
**lead** OMP session, which can start **lanes** (further OMP sessions, each in its own worktree)
for parallel work. The hub also runs scheduled jobs, posts their notices, and keeps every session
alive across restarts.

## Install

Linux x86_64 or macOS arm64, with OMP 19.1.1 or newer installed:

```sh
curl -fsSL https://raw.githubusercontent.com/vntrevx/cataly-core/main/install.sh | sh
```

`install.sh` downloads the latest release, checks its hashes and signature, and installs `cataly`.
It starts nothing and writes no configuration.

## Set up

```sh
cataly setup
```

One command does the whole first run. Discord leaves three steps to you; setup prints them:

1. In the [Developer Portal](https://discord.com/developers/applications): **New Application**,
   name it, **Create**.
2. **Bot**, **Reset Token**, copy it, and paste it when setup asks (it is not shown).
3. Open the invite link setup prints, pick your server, **Authorize**. Setup waits for the bot.

Then it fills in the rest without asking: you (the application's owner) as the owner, `#cataly`
(your talks with the lead) and `#cataly-ops` (job notices) found or created, OMP's default model
for the lead, approval buttons for OMP's permission requests (only your presses count), Cataly's
avatar and banner on the bot, a backup of the state folder to your private GitHub repository
`<gh login>/cataly-home` when `gh` is logged in, the health and pressure jobs, and a user service
(systemd on Linux, launchd on macOS) that starts the hub. It shows a short summary and asks once;
nothing changes before you say yes. Then write in `#cataly`: the lead answers there.

Running `cataly setup` again shows what is set and changes nothing. Change a default later with a
flag (`--lead-model provider/id`, `--approvals off`, `--backup off`, `--service off`,
`--owner <id>`, `--profile` to put Cataly's avatar and banner back) or by editing the config.
Scripts use `cataly setup --yes` with the same flags, and `--json`.

Put provider keys the service needs into `secrets/service.env` (setup names the missing ones).
`cataly health` shows what still needs fixing.

## Where things live

- Config: `~/.config/cataly/config.toml`, tokens in `~/.config/cataly/secrets/` (owner-only files).
- State (store, logs, files, backups): `~/.local/state/cataly`.
- Both follow `XDG_CONFIG_HOME` and `XDG_STATE_HOME`.

Every setting is described in [`docs/config.md`](docs/config.md).

## Licence

Proprietary; see [`LICENSE`](LICENSE). Third-party licences ship in `THIRD-PARTY-NOTICES.txt`
(`sh scripts/third-party-notices.sh` generates it).
