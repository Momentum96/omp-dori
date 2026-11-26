<p align="center">
  <img src="docs/cataly-portrait.webp" width="320" alt="Cataly character portrait: silver hair and a crimson halo">
</p>

<h1 align="center">Cataly</h1>

<p align="center">
  <strong>Understand the intent. Read the context. See the work through.</strong>
</p>

<p align="center">
  Cataly is an AI teammate for software work. Give it a goal and the constraints
  that matter, not a command for every step. It takes your requests in a Discord,
  Telegram or Slack room, works in tracked <a href="https://omp.sh">OMP</a> coding
  sessions on your machine, and brings back a result you can verify.
</p>

<p align="center">
  <a href="https://github.com/vntrevx/Cataly/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/vntrevx/Cataly?label=release&labelColor=2b2d33&color=c8102e"></a>
  <a href="https://www.npmjs.com/package/cataly"><img alt="npm" src="https://img.shields.io/npm/v/cataly?label=npm&labelColor=2b2d33&color=8b9099"></a>
  <a href="https://omp.sh"><img alt="OMP 19.1.1 or later" src="https://img.shields.io/badge/omp-%E2%89%A519.1.1-8b9099?labelColor=2b2d33"></a>
  <a href="LICENSE"><img alt="License: AGPL-3.0" src="https://img.shields.io/badge/license-AGPL--3.0-8b9099?labelColor=2b2d33"></a>
</p>

<p align="center">
  <a href="#get-started"><strong>Get started</strong></a> ·
  <a href="#how-it-works"><strong>How it works</strong></a> ·
  <a href="#everyday-use"><strong>Everyday use</strong></a> ·
  <a href="#help"><strong>Help</strong></a>
</p>

<p align="center">
  <strong>English</strong> · <a href="README.ko.md">한국어</a> · <a href="README.ja.md">日本語</a><br>
  <a href="README.de.md">Deutsch</a> · <a href="README.it.md">Italiano</a> · <a href="README.zh.md">中文</a>
</p>

## Give it an outcome, not a script

<table width="100%"><tr><td>

**Example request**

*Investigate the flaky login test. Keep the public API unchanged, run the
relevant tests, and open a PR with the cause and the verification results.*

</td></tr></table>

<table>
  <tr>
    <td width="33%" valign="top">
      <h4>Understand the intent</h4>
      The flaky test is the symptom. The goal is a fix you can review, with the
      public API intact. Cataly uses that distinction to choose what to
      investigate and what to leave alone.
    </td>
    <td width="33%" valign="top">
      <h4>Read the context</h4>
      The conversation, repository conventions, constraints, and your stated
      preferences inform the next step. Routine decisions belong to Cataly; real
      tradeoffs and permission decisions belong to you.
    </td>
    <td width="33%" valign="top">
      <h4>See the work through</h4>
      A patch is not the finish line. Check the agreed outcome, report the
      evidence, and say what remains unverified.
    </td>
  </tr>
</table>

The difference is not another chat interface. It is the link between the request,
its working session, and the evidence that it is done.

## How it works

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/how-it-works-dark.svg">
  <source media="(prefers-color-scheme: light)" srcset="docs/how-it-works-light.svg">
  <img src="docs/how-it-works-light.svg" width="100%" alt="Workflow: a request arrives, the lead assigns it, a lane works and reports, and completion is checked before a result you can verify, with its evidence, comes back to you.">
</picture>

1. **A request arrives.** You write in your home room (`#cataly` after setup).
   Cataly's hub runs on your machine and passes your messages, with reply
   context, to the lead.
2. **The lead takes it.** The lead is one long-running OMP session. It answers
   small requests itself, or opens a **lane**: another OMP session in its own
   worktree, with a brief and **done checks** such as "the PR is merged" or
   "this command succeeds". Each lane gets its own thread.
3. **The lane works and reports.** Lanes run in parallel and keep their sessions
   across restarts and updates, so work picks up where it was.
4. **Done is checked.** A lane saying it is done is not proof. Cataly runs the
   done checks and reports what passed and what remains unverified.

Alongside the conversation, a status room (`#cataly-ops`) keeps one live panel:
host load, sessions, provider usage, and the state of your watch jobs. Watch
jobs post what is worth knowing: GitHub repositories and pull requests, provider
quotas, host load, new mail from a command, backups and health. With voice
turned on, you can also talk to the lead in a Discord voice channel.

## You stay in control

- Cataly uses your accounts, your OMP and your permissions. Taking initiative
  does not grant permission for external actions.
- Approvals follow OMP's own settings. When OMP asks, the question comes to your
  room as buttons, and you can also answer in plain text.
- Installing starts nothing and writes no configuration. `cataly setup` shows a
  summary and changes nothing until you say yes.
- Voice is off until you turn it on, and uses your own OpenAI key.

## Get started

<table width="100%"><tr><td>

**Cataly 0.2.0 is a new product**

Releases before v0.2.0 in this repository and `cataly` npm versions before 0.2.0
are an earlier, separate product. Nothing is carried over from them: updating the
npm package replaces the old command with `cataly`. Run `cataly setup` to start fresh.

</td></tr></table>

### Requirements

- Linux or macOS, on x86_64 or arm64 (Apple Silicon and Intel Macs). Windows is not supported.
- [OMP](https://omp.sh) 19.1.1 or newer.
- A Discord bot (setup walks you through making one), or a Telegram or Slack bot.
- For the npm and Bun installs: Node.js 18 or newer on `PATH`.

### Install

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

`install.sh` downloads the newest stable release, checks its hashes and its
signature, and puts `cataly` in `~/.local/bin`. npm and Bun install the same
binary from the `cataly` package.

### First setup

```sh
cataly setup
```

Setup asks for your bot token and prints an invite link, then sets up the rest:
your home room `#cataly`, the status room `#cataly-ops`, the lead's model,
approval buttons, the default jobs, and a user service (a systemd user unit on
Linux, a launchd agent on macOS) that starts the hub. Then write in `#cataly`.

`cataly setup --voice on` adds Discord voice calls with sensible defaults; calls
cost money on your OpenAI key, capped at 30 minutes and 3 USD a day.

## Everyday use

| Command | What it does |
| --- | --- |
| `cataly status` | The hub, the lead and the lanes at a glance. |
| `cataly health` | What still needs fixing, and how. |
| `cataly update` | Stops the hub in order (a working session is never cut off), installs the new version, starts it again and resumes the lead and lanes. If the new version does not answer, it goes back. |
| `cataly rollback` | Steps back to the previous version (`install.sh` installs). |
| `cataly service disable` then `cataly uninstall` | Removes the service and the program. Your configuration (`~/.config/cataly`) and state (`~/.local/state/cataly`) are kept. |

When a new release is out, the hub tells you once in your home room. An npm or
Bun install updates through the package manager it came from.

<table width="100%"><tr><td>

**Early release**

Linux x86_64 has seen the most use; macOS and arm64 pass the same tests. Cataly is in daily use over Discord, voice included;
Telegram and Slack pass their tests but have seen less real use.

</td></tr></table>

## Help

Problems and ideas: [Issues](https://github.com/vntrevx/Cataly/issues). Please
leave tokens, IDs and private transcripts out of public reports.

Building from source and contributing: see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Cataly is open source under the [GNU AGPL 3.0](LICENSE); its source is in this
repository. Commercial licences without the AGPL obligations are available from
[vntrevx](https://github.com/vntrevx). Cataly runs on
[oh-my-pi (OMP)](https://github.com/can1357/oh-my-pi), installed separately. Third-party
licences ship with every release in `THIRD-PARTY-NOTICES-<target>.txt`.
Where the code comes from: [docs/provenance.md](docs/provenance.md).
