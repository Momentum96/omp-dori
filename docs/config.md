# Configuration reference

Everything the owner can set lives in one TOML file. This page lists every section and every key
the hub accepts. A complete example you can copy is in
[`config-example.toml`](config-example.toml); a test parses it with the hub's own parser, so it
always matches this build.

## Files and folders

| What | Where | Override |
|---|---|---|
| Config file | `~/.config/cataly/config.toml` | `$XDG_CONFIG_HOME/cataly/config.toml` |
| State folder (store `cataly.db`, `logs/`, `files/`, `backup/`, `hub.lock`) | `~/.local/state/cataly` | `$XDG_STATE_HOME/cataly` |
| Control socket | `$XDG_RUNTIME_DIR/cataly/hub.sock` | falls back to `/tmp/cataly-<uid>/hub.sock` when `XDG_RUNTIME_DIR` is unset or the path is too long for a socket |
| Token files | `secrets/` next to the config file | the `token_file` / `key_file` key of each section |

`HOME` must be set to an absolute path. An `XDG_*` variable that is not an absolute path is ignored,
as the XDG rules require. `cataly setup` shows which of these came from the environment.

The config file must be a regular file owned by you, with no access for group or others
(`chmod 600`), not a symlink, not hard-linked, UTF-8, and at most 1 MiB. A missing file means
built-in defaults for everything. `cataly setup` applies the same rule: it refuses an existing file
that breaks it and names the fix (for example `chmod go=`).

### Tokens and keys

Tokens never go into the config file. Each one sits in its own owner-only file (same rules as the
config file: yours, mode `600`, regular file, one link, at most 64 KiB, surrounding whitespace
trimmed, must not be empty). Default locations, next to the config file:

| Secret | Default file | Key that moves it |
|---|---|---|
| Discord bot token | `secrets/discord-token` | `[discord] token_file` |
| Telegram bot token | `secrets/telegram-token` | `[telegram] token_file` |
| Slack tokens, one per line: `app=xapp-…` and `api=xoxb-…` (app role) or `api=xoxp-…` (member role) | `secrets/slack-tokens` | `[slack] token_file` |
| Voice provider key | `secrets/voice-key` | `[voice] key_file` |
| Usage collector key (optional) | none | `[usage] key_file` |

Every `token_file` / `key_file` value must be an absolute path. A token file that is missing, empty,
owned by someone else or readable by others is refused, and the error names only the path.

## Checking, writing and applying the file

- `cataly config check [--file <path>]` validates a file without applying it. Validation is strict:
  an unknown key, a wrong type or an out-of-range value is an error, and every error names its
  setting path, such as `jobs.nightly.cron: never fires`. Nothing in an invalid file is applied.
- `cataly config reload` (or `SIGHUP` to the hub) reads the file again. A valid file replaces the
  running configuration as a whole; the log level changes at once, and a job whose trigger changed
  starts a new schedule. An invalid file is refused, the previous configuration stays in force, a
  `config.invalid` event is sent, and the `config` health check warns until a valid file is applied
  (then `config.valid`).
- The hub reads the file when it starts. Editing the file does nothing until a reload or restart.

### `cataly setup`

`cataly setup` writes the settings it manages into the config file without touching anything else:
comments, other keys and unknown keys stay as they are, only the managed lines change. The
settings it manages are the ones in the flag table below; the preview lists them all.

1. **Preview** (default). `cataly setup --discord-owner 1000… --discord-channels 1000…,1000…` takes
   a snapshot of the file (identity and SHA-256), shows the action (`create`, `update` or `keep`),
   the digests before and after, whether the result would be valid, and for each managed setting its
   value and where it comes from (this file and whether the running hub has applied it, or the
   built-in default). It ends with the exact command that applies it, including `--expect <hash>`.
2. **Apply.** `cataly setup --apply --expect <hash> …` refuses an invalid result, refuses when the
   file changed since the preview's hash, re-checks the snapshot just before writing, writes
   atomically (a new file is created, never overwritten), and reads it back. Your own edits made in
   between are never clobbered: setup stops and asks you to run it again.
3. Run `cataly config reload` to apply the new file to a running hub.

A new file gets `[hub] log_level = "info"` and `[omp] path = "omp"`. `--json` prints the report as
JSON. Setup refuses a file that is not a regular file, is owned by someone else, or is writable by
group or others.

| Setup option | Key it writes |
|---|---|
| `--log-level` | `[hub] log_level` |
| `--omp` | `[omp] path` |
| `--discord-token-file`, `--discord-guild`, `--discord-owner`, `--discord-channels` (comma-separated IDs) | `[discord] token_file`, `guild`, `owner`, `channels` |
| `--telegram-token-file`, `--telegram-owner`, `--telegram-group`, `--telegram-topic` | `[telegram] token_file`, `owner`, `group`, `topic` |
| `--slack-token-file`, `--slack-role`, `--slack-owner` | `[slack] token_file`, `role`, `owner` |
| `--fallback-on` | `[fallback] on` |
| `--fallback-chain` (comma-separated `provider/id`) | `[fallback.chains] default` |
| `--backup [--backup-repo owner/name] [--create]` | the `[jobs.home]` backup job (see [Home backup](#home-backup)) |

### `cataly setup discord`

The guided Discord setup. Discord leaves three steps to you, because they need your own Discord
login; setup prints them and does everything else with the bot's token:

1. In the [Developer Portal](https://discord.com/developers/applications): **New Application**,
   name it, **Create**.
2. **Bot**, **Reset Token**, copy it. At a terminal setup asks for it without showing it, checks
   it with Discord and saves it in the token file (`[discord] token_file`, default
   `secrets/discord-token`, mode `600`). A script saves it there itself. The token is never read
   from the command line.
3. Open the invite link setup prints, pick your server, **Authorize**.

What setup does, in order, reading everything again on each run:

- **Token**: `GET /users/@me`; a token Discord refuses (401) or one that is not a bot's stops the
  run and says where to reset it.
- **Message Content intent**: read from `GET /applications/@me`. When it is off, setup offers to
  switch it on (`--enable-intent` in scripts) through `PATCH /applications/@me`, which Discord
  allows for a bot in fewer than 100 servers. If Discord does not take it, setup names the place:
  Developer Portal, your application, **Bot**, **Privileged Gateway Intents**, **Message Content
  Intent**, **Save Changes**.
- **Invite link**: `https://discord.com/oauth2/authorize?client_id=<application>&scope=bot&permissions=<n>`
  with exactly the permissions Cataly uses: View Channels, Send Messages, Read Message History,
  Add Reactions, Create Public Threads, Send Messages in Threads, Manage Threads
  (`326417583168`); plus Manage Channels (`326417583184`) only when setup is to create channels.
- **Server**: `GET /users/@me/guilds`, read every 3 s until the bot joins, for at most `--wait`
  (up to `1h`; 10 minutes at a terminal, no wait in a script). With several servers setup asks
  which (`--guild <id>` in scripts; `[discord] guild` when already set).
- **Channels**: the text channels named `cataly` (your talks with the lead) and `cataly-ops` (job
  notices) are reused. Missing ones are created with `POST /guilds/{id}/channels` only after you
  confirm (`--create-channels` in scripts). No other channel is changed. Two
  text channels with the same name stop the run until you rename one.
- **Owner**: `--owner <id>`, else `[discord] owner`, else the application's owner (the team's
  owner for a team application) after you confirm (`--owner app` in scripts), else the ID you
  type.
- **Config**, through the same preview, `--expect` and apply rules as `cataly setup`:
  `[discord] guild`, `owner`, `channels` (the lead's channel added to any already listed; `home`
  set to it when the list has more than one) and `[destinations.cataly-ops]` with
  `kind = "discord"` and the `cataly-ops` channel. A setting is written only when the file says
  something else, so a second run changes nothing. At a terminal setup shows the preview and asks
  before writing; a script writes with `--apply --expect <hash>`.

`--json` never asks and prints one JSON report: the `cataly setup` report with a `discord` section
(`stage`, `next`, bot, application, `message_content`, `invite`, `guilds`, `guild`, `channels`
with `existing` or `created`, `owner` with its source). A step that still needs you ends the run
with exit status 1, `stage` naming it (`token`, `intent`, `guild`, `channels`, `owner`) and `next`
saying what to do; run setup again afterwards.

## Value formats

- **Duration**: a string of whole numbers with units `s`, `m`, `h`, `d`, summed: `"30s"`, `"5m"`,
  `"1h30m"`, `"2d"`. No decimals, no bare numbers.
- **Discord ID**: a string with the decimal ID, such as `"100000000000000001"`.
- **Model**: `provider/id`, such as `"anthropic/claude-sonnet-4-5"`; no spaces.
- **Path**: where a key says *absolute*, a relative path is an error.
- **Name** (jobs and destinations): 1 to 128 letters, digits, `.`, `@`, `_` or `-`, starting with a
  letter or digit.

---

## `[hub]`

| Key | Type | Default | Allowed | What it does | Setup |
|---|---|---|---|---|---|
| `log_level` | string | `"info"` | `error`, `warn`, `info`, `debug` | Detail of `logs/hub.log`. Changes on reload. | `--log-level` |

```toml
[hub]
log_level = "info"
```

## `[omp]`

| Key | Type | Default | Allowed | What it does | Setup |
|---|---|---|---|---|---|
| `path` | string | `"omp"` | an absolute path, or a command name without `/` looked up on `PATH` | The OMP executable the hub starts sessions with. | `--omp` |

```toml
[omp]
path = "/home/me/.bun/bin/omp"
```

## `[sessions]`

Settings for every OMP session the hub starts (lead, lanes and helper roles).

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `cap` | integer | `8` | 1–32 | Live OMP sessions at most, all roles together. |
| `credentials` | list of strings | `[]` | environment variable names: upper-case letters, digits, `_`, not starting with a digit, not starting with `CATALY_` | Variables passed from the hub's environment to OMP (provider keys). Nothing else is passed. |
| `extra_flags` | list of strings | `[]` | `--name` or `--name=value` only | Extra OMP command-line flags. Refused: flags the hub sets itself (`--mode`, `--session-dir`, `--resume`/`-r`, `--continue`/`-c`, `--restore`, `--fork`, `--print`/`-p`, `--goal`, `--plan`, `--approval-mode`, `--auto-approve`, `--yolo`, `--cwd`, `--config`, `--model`, `--thinking`, `--append-system-prompt`, `--system-prompt`, `--tools`, `--max-cost`, `--no-session`), `--extension`, and anything starting with `approval` or containing `approve`. |
| `spend_phrases` | list of strings | `[]` | any; matched case-insensitively, blanks dropped | Extra phrases that mark an OMP dialog as an offer to spend money, beside the built-in ones. Such dialogs are never accepted for you. |
| `runtime` | string | `"headless"` | `headless`, `tern` | Where new sessions run. A stored session keeps the runtime it started with. |
| `tern` | string | `"tern"` | non-blank; absolute path or name on `PATH` | The Tern executable, used when `runtime = "tern"`. |
| `tern_session` | string | none (the session shown) | any non-blank name | The Tern session new blocks open in. |

```toml
[sessions]
cap = 6
credentials = ["ANTHROPIC_API_KEY"]
extra_flags = ["--no-skills"]
```

## `[lead]`

Present means the hub keeps one standing lead session. Absent means no lead.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `model` | string | **required** | `provider/id` | The lead's model. |
| `thinking` | string | OMP's default | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `auto` | Thinking level. |
| `cwd` | string | your home folder | absolute path | Working folder of the lead. |
| `session_id` | string | none | any non-blank | The lead resumes exactly this OMP session. |
| `instructions` | string | none | absolute path to an owner-only file | Role instructions appended to the lead's system prompt. |
| `autostart` | boolean | `true` | | Start the lead when the hub starts. |

```toml
[lead]
model = "anthropic/claude-sonnet-4-5"
thinking = "medium"
cwd = "/home/me/work"
```

## `[fallback]`

What OMP may do when a model fails. Written into OMP's settings for hosted sessions only when set;
unset keeps OMP's own setting. A fallback only ever moves to another configured model; it never
redeems a reset, buys credits or turns on overage.

| Key | Type | Default | Allowed | What it does | Setup |
|---|---|---|---|---|---|
| `on` | string | unset | `any`, `usage-limit`, `except-usage-limit` | Which errors make OMP fall back (OMP `retry.fallbackOn`). | `--fallback-on` |
| `chains.<role>` | list of strings | none | `provider/id` models | Ordered models OMP may move to for that role (OMP `retry.fallbackChains`). | `--fallback-chain` writes `chains.default` |

```toml
[fallback]
on = "usage-limit"

[fallback.chains]
default = ["anthropic/claude-sonnet-4-5", "openai/gpt-5"]
```

## `[discord]`

The Discord bot: where its token is, and intake (which messages reach the lead). Intake runs only
when both `owner` and `channels` are set; setting one without the other is an error.

| Key | Type | Default | Allowed | What it does | Setup |
|---|---|---|---|---|---|
| `token_file` | string | `secrets/discord-token` | absolute path | The bot token file. | `--discord-token-file` |
| `guild` | string | none | Discord ID | Your server. Needed for destinations that name a channel by name, and for voice. | `--discord-guild`, `setup discord` |
| `api_base` | string | Discord's API | `http(s)://` on localhost only | Test override of the API address. | |
| `owner` | string | none | Discord user ID | Your account. | `--discord-owner`, `setup discord` |
| `channels` | list of strings | `[]` | Discord IDs, at most 256, duplicates dropped | Watched channels, threads, DMs and forums. | `--discord-channels`, `setup discord` |
| `follow_threads` | boolean | `true` | | Also take threads under watched channels. | |
| `home` | string | the one watched channel, if there is exactly one | one of `channels` | Where the lead's questions go. | `setup discord` |
| `remote_approvals` | boolean | `false` | | Let you approve OMP permission requests by button. | |
| `form_default_after` | duration | `"3m"` | 1s–1h | A question with one recommended choice takes it after this long. | |
| `profile` | boolean | `true` | | Clear the bot's avatar at start. | |
| `away` | duration | `"3h"` | 1s–720h | You count as back when your message comes this long after your previous one. | |
| `others` | list of strings | `[]` | subset of `channels` | Watched rooms where you talk with other people. | |
| `others_alert` | duration | `"120m"` | 1s–168h | A message from someone else in those rooms unanswered this long is reported once. | |
| `others_drop` | duration | `"24h"` | 1s–720h | Such entries older than this are dropped silently. | |

```toml
[discord]
owner = "100000000000000001"
channels = ["100000000000000010"]
```

## `[telegram]`

Intake runs when `owner` is set. IDs may be written as numbers or as strings.

| Key | Type | Default | Allowed | What it does | Setup |
|---|---|---|---|---|---|
| `token_file` | string | `secrets/telegram-token` | absolute path | The bot token file. | `--telegram-token-file` |
| `api_base` | string | Telegram's Bot API | localhost only | Test override. | |
| `owner` | integer or string | none | positive user ID | Your account; your DM with the bot is taken. | `--telegram-owner` |
| `group` | integer or string | none | negative chat ID | The one group or supergroup taken besides your DM. | `--telegram-group` |
| `topic` | integer or string | none | positive; needs `group` | The one forum topic of that group taken (`1` is General). | `--telegram-topic` |
| `chats` | list | `[]` | non-zero chat IDs | Further chats the hub may send to. | |
| `profile` | boolean | `true` | | Clear the bot's name at start. | |

```toml
[telegram]
owner = 100000001
group = -1000000000001
topic = 1
```

## `[slack]`

Intake runs when `owner` is set. Only your messages, and those of members in `allow`, are taken.

| Key | Type | Default | Allowed | What it does | Setup |
|---|---|---|---|---|---|
| `token_file` | string | `secrets/slack-tokens` | absolute path | The tokens file (see [Tokens](#tokens-and-keys)). | `--slack-token-file` |
| `api_base` | string | Slack's API | localhost only | Test override. | |
| `role` | string | `"app"` | `app`, `member` | `app`: the bot user, sees only conversations it is in. `member`: the app's user token, sees what that member sees (needs a paid seat). | `--slack-role` |
| `owner` | string | none | member ID starting with `U` or `W`, such as `U0123ABCD` | Your account. | `--slack-owner` |
| `allow` | list of strings | `[]` | Slack member IDs | Further members whose messages are taken. | |

```toml
[slack]
role = "app"
owner = "U0000OWNER"
```

## `[context]`

Extra lines added to requests sent to the lead. Both are off by default.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `facts` | boolean | `false` | | Add a line of channel facts. |
| `lookup` | boolean | `false` | | Add lines from earlier conversation in the room. |
| `lookup_days` | integer | `30` | 1–3650 | Stored rows for the lookup are kept this many days. |
| `notes.<relay>:<room>` | string | none | key: `discord:`, `telegram:` or `slack:` followed by the room ID; value: one line, 1–500 characters, no control or direction-override characters | Your one-line note about that room, shown with its requests. |

```toml
[context]
facts = true

[context.notes]
"discord:100000000000000010" = "the release room"
```

## `[voice]` and `[transcribe]`

`[voice]` is the Discord voice assistant. It is off unless `enabled = true`; while off, its other
keys are not read, but a key that is not in the table below is an error at load time either way.
When enabled, a missing or invalid voice key turns voice off and names the
field, but the rest of the configuration still applies. A build without the `voice` feature refuses
`enabled = true` the same way. Voice also needs `[discord] guild` and `[discord] owner`.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `enabled` | boolean | `false` | | Turns voice on. |
| `voice_channel` | string | **required** | Discord ID | Voice channel to join. |
| `text_channel` | string | **required** | Discord ID | Text channel for the call's text. |
| `speech_model` | string | **required** | 1–64 letters, digits, `.`, `-`, `_` | Realtime speech model. |
| `backend_model` | string | **required** | same | Model that answers. |
| `summary_model` | string | **required** | same | Model that summarises. |
| `voice` | string | **required** | same | Voice name. |
| `backend_prices` | table | **required** | `input`, `cached_input`, `cache_write`, `output`: USD per million tokens, 0 or more, all required | Prices of the backend model, for the spending cap. |
| `summary_prices` | table | **required** | same | Prices of the summary model. |
| `search_price` | number | **required** | USD, 0 or more | Price of one web search. |
| `speech_price_per_minute` | number | none | USD, 0 or more | Price of speech per minute. |
| `reasoning_effort` | string | `"low"` | `minimal`, `low`, `medium`, `high` | Reasoning effort of the backend model. |
| `preferences` | string | `""` | any | Your standing preferences for the assistant. |
| `zone` | string | `"UTC"` | IANA zone | Time zone for the daily cap. |
| `daily_minutes` | integer | `30` | 1–1440 | Call minutes per day. |
| `total_minutes` | integer | `0` (no total cap) | 0–4294967295 | Call minutes in all. |
| `spending_cap_usd` | number | `3.0` | above 0 | Spending cap. |
| `idle_close_seconds` | integer | `120` | 1–3600 | Leave the call after this much silence. |
| `wake_dbfs` | number | `-45.0` | between -120 and 0, exclusive | Sound level that counts as speech. |
| `transport` | string | `"websocket"` | `websocket`, `webrtc` | Connection to the speech provider. |
| `api_base` | string | `"https://api.openai.com"` | URL | Speech provider address. |
| `key_file` | string | `secrets/voice-key` | absolute path | Provider key file. |

`[transcribe]` is the owner command behind `cataly transcribe <file|url>`; the audio file's path is
appended to it. A URL is downloaded first.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `command` | list of strings | none | program first, non-empty | The transcription command. |
| `max_mib` | integer | `25` | 1–1024 | Largest download, in MiB. |

```toml
[transcribe]
command = ["/usr/local/bin/whisper-cli", "--model", "base"]
```

## `[lanes]`

Defaults for lanes (worker sessions the lead opens) and the rules they run under.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `model` | string | OMP's default | `provider/id` | Model of new lanes. |
| `cwd` | string | none | absolute path | Default working folder of new lanes. |
| `max` | integer | `20` | 0–1000; `0` = no limit | Live lanes at most. |
| `start_budget` | duration | `"180s"` | 10s–30m | How long a lane may take to start. |
| `objection` | duration | `"5m"` | 1s–24h | Window in which you can object to a proposed lane. |
| `ceiling_minutes` | number | no ceiling | finite, 0 or more | Default run-time ceiling per lane. |
| `ceiling_tokens` | number | no ceiling | finite, 0 or more | Default token ceiling per lane. |
| `ceiling_cost` | number | no ceiling | finite, 0 or more | Default cost ceiling per lane. |
| `post_kinds` | list of strings | all five | `progress`, `stuck`, `waiting`, `stopped`, `finished` | Report kinds posted to the lane's thread automatically. |
| `post_raw` | boolean | `true` | | Post the lane's own report text (else only summaries). |
| `thread_member` | boolean | `true` | | Add you to lane threads the hub creates. |
| `identities.<owner/name>` | string | none | an email address | The commit identity the policy check expects in that repository. |

`[lanes.oversight]`: when the hub nudges a lane. The reminders and the stall alert must come in
order: `first_reminder` < `second_reminder` < `stall`.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `enabled` | boolean | `true` | | Turns oversight on. |
| `first_reminder` | duration | `"10m"` | 1s–24h | First reminder to a silent lane. |
| `second_reminder` | duration | `"20m"` | 1s–24h | Second reminder. |
| `stall` | duration | `"40m"` | 1s–24h | The lane is reported stalled. |
| `busy` | duration | `"20m"` | 1s–24h | A lane working without a break this long… |
| `quiet` | duration | `"20m"` | 1s–24h | …and without progress this long is judged stuck. |
| `sweep` | duration | `"5m"` | 1s–24h | How often oversight looks. |
| `thread_quiet` | duration | `"20m"` | 1s–24h | A progress report posts to the thread only after this quiet time. |

`[lanes.shell]`: shell rules for lanes. Patterns are regular expressions matched against the whole
command.

| Key | Type | Default | What it does |
|---|---|---|---|
| `deny` | list of patterns | `[]` | Commands no lane may run. |
| `allow` | list of tables | `[]` | Each `{ lanes = "<glob>", patterns = [...] }` lets lanes whose key matches the glob (only `*` is special; `lanes` is required) run matching commands. |

```toml
[lanes]
model = "anthropic/claude-sonnet-4-5"
max = 4

[lanes.shell]
deny = ["git push --force.*"]

[[lanes.shell.allow]]
lanes = "web-*"
patterns = ["git push origin web-.*"]
```

## Coordination: `[coord]`

Named slots, the planner, the reviewer, lane event commands, publish folders and boards.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `board` | list of strings | `[]` | `discord:<channel id>` (17–20 digits), `telegram:<chat>[/<topic>]`, `slack:<workspace>/<conversation>` | Where the board is posted. |

`[coord.slots]`

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `count` | integer | `1` | 1–64 | Slots per name unless `sizes` names it. |
| `load` | integer | `80` | 1–10000 | Slot grants wait while the one-minute load per core is at or above this per cent. |
| `sizes.<name>` | integer | `count` | 1–64; name: letter or digit, then letters, digits, `_`, `.`, `-`, at most 64 | Slots for that name. |

`[coord.planner]`

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `enabled` | boolean | `true` | | Turns the planner on. |
| `interval` | duration | `"1h"` | 1m–24h | Time between planner passes. |
| `max_open` | integer | `3` | 0–1000 | Planned lanes open at once at most. |
| `max_starts` | integer | `2` | 0–100 | Lanes started per pass at most. |
| `notes_hour` | integer | none | 0–23 | Local hour of the daily owner-notes refresh. |
| `import_label` | string | `"cataly"` | | GitHub issue label imported as work items. |
| `decision_label` | string | `"needs-decision"` | | Label marking an item that needs your decision. |
| `queue_expiry` | duration | `"24h"` | 1h–720h | A queued launch expires after this. |

`[coord.review]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `model` | string | OMP chooses | Model of the built-in reviewer. |
| `command` | list of strings | none | A custom reviewer command that replaces the built-in one. Placeholders as for events. |

`[coord.events]`: commands run on lane events, never through a shell. Each is a non-empty list of
non-empty strings; elements may contain `{key}`, `{text}`, `{evidence}`, `{pr}`, `{repo}`,
`{number}`, `{head}`.

| Key | What triggers it |
|---|---|
| `finished` | A lane finished. |
| `report` | A lane reported. |
| `dead` | A lane died. |

`[[coord.publish]]`: folders whose files may be published, each a table.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `dir` | string | **required** | absolute path | The folder. |
| `url` | string | none | `http(s)://` without credentials, query or fragment | Its public address. |

```toml
[coord]
board = ["discord:100000000000000011"]

[coord.slots]
count = 2
sizes = { build = 1 }

[coord.planner]
max_open = 2
```

## `[outgoing]`

Checks on text the hub sends. Text with an unfilled `{field}`, a placeholder word or a banned
character is refused and the reason names what matched.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `placeholder_words` | list of strings | `[]` | any; matched case-insensitively | Words that must not appear (`TODO`, `lorem`). |
| `banned_characters` | list of strings | `[]` | each exactly one character | Characters that must not appear. |

```toml
[outgoing]
placeholder_words = ["TODO", "lorem ipsum"]
banned_characters = ["—"]
```

## `[limits]`

The account-limits view (`cataly limits`), and the cache the quota job shares.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `enabled` | boolean | `false` | | Turns the view on; off, `cataly limits` is refused. |
| `cache` | duration | `"5m"` | 1m–24h | How long fetched usage data counts as fresh. |
| `warn_percent` | integer | none | 1–100 | Shows each window's headroom to this level. |

```toml
[limits]
enabled = true
warn_percent = 80
```

## `[health]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `ignore` | list of strings | `[]` | Check names left out of the health status and the `health` job, such as `bridge`, `omp`, `config`, `discord.token`, `job.<name>`, `file.<path>`. |

```toml
[health]
ignore = ["bridge"]
```

## `[update]` and `[usage]`

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `update.check` | boolean | `true` | | Daily check for a newer stable release; off means no request is ever made. |
| `update.first_after` | duration | `"10m"` | 1s–24h | First check after the hub starts. |
| `update.api_base` | string | GitHub's API | localhost only | Test override. |
| `usage.enabled` | boolean | `false` | needs `endpoint` | Sends usage counts. There is no default collector. |
| `usage.endpoint` | string | none | URL | OTLP/HTTP logs endpoint, such as `https://collector.example/v1/logs`. |
| `usage.key_file` | string | none | absolute path | Owner-only file with the collector's key, sent as a bearer token. |

```toml
[update]
check = true
```

---

## `[destinations.<name>]`

Where job events go besides the store and the event log. The name `log` is reserved: it is the
built-in local destination and may be listed in a job's `destinations`. Every destination needs
`kind`.

| Kind | Keys | Allowed |
|---|---|---|
| `command` | `argv` (**required**) | non-empty list of non-empty strings. The command runs with the event as JSON in the `CATALY_EVENT` environment variable. |
| `discord` | `channel` (**required**) | a channel or thread ID, or a channel name (`#name` or `name`) when `[discord] guild` is set |
| `telegram` | `chat` (**required**), `topic` | `chat`: non-zero chat ID; `topic`: 1–2147483647. Both as an integer or a string, as in `[telegram]` |
| `slack` | `channel` (**required**), `thread` | `channel`: `<workspace>/<conversation>`, such as `T0123ABCD/C0456EFGH`; `thread`: a message timestamp such as `1700000000.000100` |

```toml
[destinations.phone]
kind = "discord"
channel = "100000000000000012"

[destinations.pager]
kind = "command"
argv = ["/usr/local/bin/notify"]
```

## `[jobs.<name>]`

Every job has `kind` and these common keys:

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `kind` | string | **required** | `cron`, `once`, `probe`, `github`, `pr`, `quota`, `backup`, `health`, `items`, `pressure` | What the job is. |
| `enabled` | boolean | `true` | | Off keeps the job but never runs it. |
| `dry_run` | boolean | `false` | | Trial mode: command jobs run with `CATALY_DRY=1`, and the job keeps its state apart from live runs; that trial state is discarded when `dry_run` is turned off. |
| `timeout` | duration | `"30s"` for command jobs; per kind below for owner jobs | 1s–24h | Time limit of a run, or of one tick. |
| `destinations` | list of strings | `[]` | names of `[destinations.*]`, or `log` | Where the job's events go. |
| `templates` | table | built-in wording | keys: event names of this job; values: text with `{field}` slots (`{{` / `}}` for literal braces) | Your own wording per event. |

Command jobs (`cron`, `once`, `probe`) also run with `CATALY_JOB=<name>` set.

### Command jobs

| Kind | Keys | Allowed |
|---|---|---|
| all three | `argv` (**required**), `cwd` (**required**) | `argv`: non-empty list of non-empty strings, no shell; `cwd`: absolute path |
| `cron` | `cron` (**required**), `zone` (**required**) | five-field cron with month and weekday names, lists, ranges, steps (`?` is not supported); an IANA zone. A local time skipped by daylight saving does not fire; a repeated one fires once. An expression that never fires is refused. |
| `once` | `at` (**required**) | an instant with seconds and offset, such as `2026-12-01T09:00:00Z` |
| `probe` | `every` (**required**), `fire`, `skip_identical` | `every`: 30s–24h; `fire`: `change` (default: on the first success after a failure) or `every` (each success); `skip_identical`: boolean, default `false` |

Events: `cron` and `once` emit `run.done`, `run.skipped`, `run.broken`; `probe` emits
`probe.fired`, `probe.error`, `probe.recovered`, `run.broken`.

| Event | Template fields |
|---|---|
| `run.done` | `job`, `outcome`, `exit`, `signal`, `timed_out`, `bytes`, `log`, `scheduled` |
| `run.skipped` | `job`, `count`, `reason` |
| `run.broken` | `job`, `run`, `started` |
| `probe.fired` | `job`, `bytes`, `log`, `scheduled` |
| `probe.error` | `job`, `reason`, `run`, `log` |
| `probe.recovered` | `job`, `errors` |

```toml
[jobs.nightly]
kind = "cron"
cron = "0 3 * * *"
zone = "Europe/Berlin"
argv = ["/home/me/bin/nightly.sh"]
cwd = "/home/me"
timeout = "10m"
destinations = ["phone", "log"]        # "phone" from the [destinations.phone] example above
templates = { "run.done" = "{job} finished: {outcome}" }
```

### Owner jobs

Built-in kinds. Each ticks every `every`, the first tick when the job is registered. All of them
emit `job.failed` (fields `job`, `reason`) and `job.recovered` (`job`, `failures`) besides their
own events.

| Kind | `every` default | `every` minimum | `timeout` default |
|---|---|---|---|
| `github` | 3m | 1m | 3m |
| `pr` | 3m | 1m | 3m |
| `quota` | 5m | 2m | 30s |
| `backup` | 30m | 5m | 10m |
| `health` | 2m | 1m | 60s |
| `items` | 30m | 5m | 60s |
| `pressure` | 60s | 30s | 30s |

The maximum `every` is 24h for all.

#### `github`: watch repositories

Discovers the newest pull requests of watched repositories and authors through `gh`, follows them,
and reports participating notifications (mention, team mention, review requested) once each.
Your own activity is never reported as someone else's.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `repos` | list of tables | **required** | each `{ repo = "owner/name", authors = [...] }`; `repo` required (letters, digits, `-`, `_`, `.`), `authors` optional logins | Repositories to watch, optionally only PRs by these authors. |
| `notifications` | boolean | `true` | | Report participating notifications for watched repositories. |
| `gh` | string | `"gh"` | | The `gh` executable. |
| `login` | string | read once from `gh` | | Your GitHub login. |
| `bots` | list of strings | `[]` | | Bot logins that count as you. |
| `emails` | list of strings | `[]` | | Commit emails that count as you. |

Events: `github.pr` (`repo`, `number`, `title`, `state`, `author`, `url`),
`github.notification` (`repo`, `reason`, `title`, `kind`), and the `pr.*` events below.

#### `pr`: follow named pull requests

Follows the PRs you add with `cataly pr follow`: new feedback from someone else, newly failed checks,
pushes, merge, close and reopen. Keys: `gh`, `login`, `bots`, `emails` as for `github`.

Events: `pr.baseline` (`pr`, `title`, `state`), `pr.feedback` (`pr`, `author`, `what`, `excerpt`,
`url`), `pr.check_failed` (`pr`, `check`, `conclusion`, `head`), `pr.pushed` (`pr`, `authors`,
`count`, `old`, `new`, `rewritten`), `pr.merged` (`pr`, `commit`), `pr.closed` (`pr`),
`pr.reopened` (`pr`).

#### `quota`: provider quota and the quota gate

Warns once when a provider limit rises past an alert level. While any watched limit is at or above
`hold`, the quota gate holds new session starts; it opens when fresh data shows all of them below,
or when every limit that closed it has passed its reset time. `cataly quota lift` opens it by hand.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `providers` | list of strings | `[]` (all OMP reports) | | Providers watched. |
| `levels` | list of integers | `[70, 85, 95]` | 1–100, at least one; sorted, duplicates dropped | Alert levels in per cent. |
| `hold` | integer | `85` | 1–100 | The gate closes at this level. |

Events: `quota.level` (`limit`, `provider`, `account`, `percent`, `window`, `reset_in`),
`quota.recovered` (`limit`, `provider`, `account`, `percent`), `quota.hold` (`limits`, `level`),
`quota.released` (`reason`).

#### `backup`: commit and push a folder

Stages every change in a git working tree, scans it for secrets (built-in token shapes and your
patterns) and for files over the size limit, and only then commits and pushes. Never force-pushes,
rewrites history or pulls. A refusal commits nothing and names files and rule, never the matched
text. The config file and `secrets/` inside the tree are refused unless git ignores them.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `dir` | string | **required** | absolute path | The working tree. |
| `name` | string | **required** | | Commit author name. |
| `email` | string | **required** | | Commit author email. |
| `remote` | string | `"origin"` | | Remote pushed to. |
| `branch` | string | `"main"` | | Branch pushed to. |
| `patterns` | list of strings | `[]` | regular expressions | Your secret patterns. |
| `max_file_mib` | integer | `50` | 1–4096 | Largest file committed, in MiB. |
| `include_cataly` | boolean | `false` | | Before each tick, copy the config file and a consistent store snapshot into the tree (the home backup). |

Events: `backup.refused` (`files`, `rule`), `backup.clean` (`commit`),
`backup.push_failed` (`reason`).

##### Home backup

`cataly setup --backup` backs up the state folder to a private GitHub repository, by default
`<your gh login>/cataly-home` (`--backup-repo owner/name` picks another). The preview reads your
`gh` login and the repository; a public repository is refused, and a missing one is created private
only with `--apply --create`. Applying makes the state folder a git working tree whose
`.gitignore` keeps token files, secrets, keys and the live database out, sets the remote, and adds
`[jobs.home]` with `kind = "backup"`, `dir` = the state folder, `remote = "origin"`,
`branch = "main"`, your GitHub name and no-reply email, and `include_cataly = true`.

#### `health`: report problems

Runs the same checks as `cataly health` and reports a warning only after `strikes` consecutive bad
ticks, then its clearing once. Checks in `[health] ignore` are skipped.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `strikes` | integer | `2` | 1–5 | Consecutive bad ticks before a problem is reported. |
| `files` | list of tables | `[]` | each `{ path = "<absolute>", max_age = "12h" }`; `max_age` 1m–8760h, default 12h | Files that must have changed within `max_age` (check `file.<path>`). |

Events: `health.problem` (`check`, `detail`), `health.cleared` (`check`).

#### `items`: new items from a command (mail watch)

Runs your command each tick. It prints one item per line: an identity, a tab, a title (at most 100
lines, 64 KiB). Each identity not seen before is reported once; the first tick only records what is
there. The command runs at most 60 s and never longer than `timeout`.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `argv` | list of strings | **required** | non-empty, no shell | The command, such as a mail search. |
| `cwd` | string | `"/"` | absolute path | Its working folder. |

Events: `items.new` (`job`, `id`, `title`).

#### `pressure`: host load, memory and disk

Samples the host. Any failing check holds new session starts, and an unknown value always holds.
Tells the lead once when the host enters alert and once when it clears, and once when it becomes
ready for heavy compute and once when it is busy again. Without a pressure job, session starts
use the same rule with the defaults below. `alert_load` must be greater than `ready_load`.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `alert_load` | integer | `150` | 1–10000 | Load per core, in per cent, above which the host is in alert. |
| `ready_load` | integer | `80` | 1–10000 | Load per core below which heavy compute is welcome. |
| `min_free_memory` | integer | `20` | 0–100 | Free memory (and container headroom) share, in per cent, below which starts wait. |
| `min_free_disk_gb` | integer | `50` | 0–1000000 | Free disk in GB below which starts wait. |
| `disk_path` | string | your home folder | absolute path | File system checked for disk space. |
| `max_sessions` | integer | `20` | 0–1000; `0` = off | Live lane sessions at or above this hold starts. |

Events: `pressure.alert` (`reasons`), `pressure.clear`, `pressure.ready` (`load`),
`pressure.busy` (`load`).

```toml
[jobs.host]
kind = "pressure"
min_free_disk_gb = 20
max_sessions = 6
```
