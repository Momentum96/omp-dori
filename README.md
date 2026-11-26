# Cataly

Cataly is one Rust hub that runs on your machine over OMP (the coding agent).
You talk to it from a messenger (Discord, Telegram or Slack): your messages go to a long-running
**lead** OMP session, which can start **lanes** (further OMP sessions, each in its own worktree)
for parallel work. The hub also runs scheduled jobs, posts their notices, and keeps every session
alive across restarts.

This repository holds Cataly's source. Releases, install instructions and the project's front page
live in [`vntrevx/Cataly`](https://github.com/vntrevx/Cataly).

## Build from source

Needs the Rust toolchain pinned in `rust-toolchain.toml`, and [Bun](https://bun.sh) for the OMP
extension in `extension/`.

```sh
cargo build --release                    # add --features voice for Discord voice
cargo test --locked --no-fail-fast
cd extension && bun install --frozen-lockfile && bun test
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full set of checks and how to contribute.

## Install

Linux x86_64 (macOS arm64 once its build is published), with OMP 19.1.1 or newer installed; an npm
or Bun install also needs Node.js 18 or newer on PATH (the `cataly` launcher runs under `node`);
`install.sh` does not need it. Releases tagged before v0.2.0 in `vntrevx/Cataly` are an earlier,
separate product. One of:

```sh
curl -fsSL https://raw.githubusercontent.com/vntrevx/Cataly/main/install.sh | sh
npm install --global cataly
bun add --global cataly
```

`install.sh` picks the newest stable Cataly release on GitHub (`vntrevx/Cataly`; prereleases and
releases of other software there are skipped), checks its hashes and signature, and installs
`cataly` into the program folder with the command link `~/.local/bin/cataly`. npm and Bun install
the `cataly` package, whose launcher runs the binary from the platform package
`cataly-linux-x64` (or `cataly-darwin-arm64`) beside it; that package also holds the OMP
extension, and the hub links it into OMP when it starts. None of them starts anything or writes
configuration.

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
(your talks with the lead) and `#cataly-ops` found or created, OMP's default model
for the lead, approval buttons for OMP's permission requests (only your presses count), Cataly's
avatar and banner on the bot (the avatar also as the application's icon), a backup of the state folder to your private GitHub repository
`<gh login>/cataly-home` when `gh` is logged in, the health and pressure jobs, and a user service
(systemd on Linux, launchd on macOS) that starts the hub. It shows a short summary and asks once;
nothing changes before you say yes. Then write in `#cataly`: the lead answers there.
`#cataly-ops` holds the status panel and the few alerts worth a ping, kept apart from your talks
with the lead (see [`[ops]`](docs/config.md#ops)).

Running `cataly setup` again shows what is set and changes nothing. Change a default later with a
flag (`--lead-model provider/id`, `--approvals off`, `--backup off`, `--service off`,
`--owner <id>`, `--profile` to put Cataly's avatar and banner back) or by editing the config.
Scripts use `cataly setup --yes` with the same flags, and `--json`.

Put provider keys the service needs into `secrets/service.env` (setup names the missing ones).
`cataly health` shows what still needs fixing.

## Update

When a new release is out, the hub tells you once in your home room. Then run, from any
terminal:

```sh
cataly update
```

It downloads and checks the release, asks the running hub to stop in order, switches to the new
version, starts the hub the way it ran before (the user service, or in the background), and waits
30 s for the new version to answer; otherwise it switches back. The new hub resumes the lead and
every lane on their own session files, so their conversations and goals carry on (a paused lane
stays paused), and posts once in your home room that it was updated and which sessions it
resumed. A session that is working or waiting for your answer is never stopped: the update waits
up to 60 s, then says which session it waits for and changes nothing; run it again once that work
is done. OMP sessions you started yourself are not the hub's and are never touched.

`cataly update --no-restart` installs without touching a running hub; the new version runs from
the hub's next start. `cataly rollback` goes back to the previous version the same way.

An npm or Bun install updates the same way, but the package manager that installed it does the
install step: `cataly update` sees that it runs from `node_modules/cataly-<platform>`, stops the
hub in order, runs `npm install --global cataly@<version>` (into the same npm prefix) or
`bun add --global cataly@<version>` (with the same `BUN_INSTALL`), and starts the new hub from the
same path; if the new version does not come up, the earlier one is installed again. The user
service runs the binary inside the package folder, a path the next npm or Bun upgrade keeps.
Such an install keeps no previous version: `cataly rollback` names the install command to use
instead.

## Uninstall

Stop the hub (`cataly service disable` stops and removes the service setup made), then run
`cataly uninstall`. It removes the extension link and, for an `install.sh` install, the program
folder and the command link; for an npm or Bun install it names `npm uninstall --global cataly`
or `bun remove --global cataly`. Config and state are kept.

## Where things live

- Config: `~/.config/cataly/config.toml`, tokens in `~/.config/cataly/secrets/` (owner-only files).
- State (store, logs, files, backups): `~/.local/state/cataly`.
- Both follow `XDG_CONFIG_HOME` and `XDG_STATE_HOME`.

Every setting is described in [`docs/config.md`](docs/config.md).

## Licence

Copyright (c) 2026 vntrevx. Cataly is licensed under the GNU Affero General Public License,
version 3 only (AGPL-3.0-only); see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE). Commercial
licences without the AGPL obligations are available from vntrevx; contact
[vntrevx on GitHub](https://github.com/vntrevx). Contributions need the [CLA](CLA.md).

Third-party licences ship in `THIRD-PARTY-NOTICES.txt` (`sh scripts/third-party-notices.sh`
generates it).
