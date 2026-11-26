![Cataly](https://raw.githubusercontent.com/vntrevx/Cataly/main/assets/banner.jpg)

# Cataly

Cataly is a hub that runs on your machine over [OMP](https://omp.sh), the coding agent, so you can
work with OMP from a messenger.

- **Home room.** You write in a Discord, Telegram or Slack room; your messages go to the hub.
- **Lead session.** A long-running OMP session answers you there and keeps its conversation
  across restarts and updates.
- **Lanes.** The lead can start further OMP sessions, each in its own worktree, for parallel work.
- **Watch jobs.** Scheduled jobs post what is worth knowing: GitHub repositories and pull requests,
  provider quotas, host load, new mail from a command, backups, health.
- **Voice.** Talk to the assistant in a Discord voice channel (off until you turn it on).

This repository holds releases only. Releases tagged before v0.2.0 here (the OMP-Dori
pre-releases) belong to an earlier, separate product and are not Cataly.

## Requirements

- Linux x86_64 (macOS arm64 is coming).
- [OMP](https://omp.sh) 19.1.1 or newer.
- A Discord bot; setup walks you through making one.
- For an npm or Bun install: Node.js 18 or newer on PATH (the `cataly` launcher runs under
  `node`). `install.sh` does not need Node.js.

## Install

Pick one:

```sh
curl -fsSL https://raw.githubusercontent.com/vntrevx/Cataly/main/install.sh | sh
```

```sh
npm install --global cataly
```

```sh
bun add --global cataly
```

`install.sh` downloads the newest stable release, checks its hashes and its signature, and puts
`cataly` in `~/.local/bin`. npm and Bun install the same binary from the `cataly` package. None of
them starts anything or writes any configuration.

## First setup

```sh
cataly setup
```

Setup asks for your Discord bot token and prints an invite link, then sets up the rest: your home
room `#cataly`, a status room `#cataly-ops`, the lead's model, approval buttons for OMP's
permission requests, the default jobs, and a user service (systemd) that starts the hub. It shows
a summary and changes nothing until you say yes. Then write in `#cataly`.

`cataly health` shows what still needs fixing.

## Update

When a new release is out, the hub tells you once in your home room. Then run:

```sh
cataly update
```

It stops the hub in order (a session that is working is never cut off), installs the new version,
starts the hub again and waits for it to answer; the lead and lanes resume where they were. If the
new version does not come up, it goes back to the one before. An npm or Bun install updates
through the package manager it came from; an `install.sh` install can also step back with
`cataly rollback`.

## Uninstall

```sh
cataly service disable    # stops and removes the user service, if setup made one
cataly uninstall
```

Stop a hub you started by hand first. `cataly uninstall` removes the OMP extension link and, for
an `install.sh` install, the program; for an npm or Bun install it names the command that removes
the package (`npm uninstall --global cataly` or `bun remove --global cataly`). Your configuration
(`~/.config/cataly`) and state (`~/.local/state/cataly`) are kept.

## Licence

Cataly is proprietary software; see [LICENSE](LICENSE). For licensing, contact
[vntrevx on GitHub](https://github.com/vntrevx). Third-party licences ship with every release in
`THIRD-PARTY-NOTICES.txt`.
