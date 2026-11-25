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

## Set up Discord

```sh
cataly setup discord
```

Discord leaves three steps to you; setup prints them and does the rest:

1. In the [Developer Portal](https://discord.com/developers/applications): **New Application**,
   name it, **Create**.
2. **Bot**, **Reset Token**, copy it, and paste it when setup asks (it is not shown).
3. Open the invite link setup prints, pick your server, **Authorize**.

Setup then waits for the bot to join, finds or creates `#cataly` (your talks with the lead) and
`#cataly-ops` (job notices), asks for the lead's model and whether to approve permission requests
by button, previews the config and writes it when you agree.

## Run it

```sh
cataly service enable
```

This writes and starts a user service (systemd on Linux, launchd on macOS). Put provider keys the
service needs into `secrets/service.env` (it tells you which are missing). Then write in
`#cataly`: the lead answers there. `cataly health` shows what still needs fixing.

## Where things live

- Config: `~/.config/cataly/config.toml`, tokens in `~/.config/cataly/secrets/` (owner-only files).
- State (store, logs, files, backups): `~/.local/state/cataly`.
- Both follow `XDG_CONFIG_HOME` and `XDG_STATE_HOME`.

Every setting is described in [`docs/config.md`](docs/config.md).

## Licence

Proprietary; see [`LICENSE`](LICENSE). Third-party licences ship in `THIRD-PARTY-NOTICES.txt`
(`sh scripts/third-party-notices.sh` generates it).
