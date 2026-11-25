# Milestone 7 build notes (branch feat/release)

## Done

- `sys::release`: version report (`cataly version --json`), asset names (`cataly-<v>-<target>`,
  `cataly-<v>.ts`, fixed `install.sh`, `SHA256SUMS`, `SHA256SUMS.sig` as in DESIGN 11.1), checksum
  list parsing (exactly one line per asset), ed25519 signature check with ring, executable-format
  check, GitHub release metadata (15 s, 1 MiB) and capped downloads that follow redirects to HTTPS
  only (`relay::http::download`, the one HTTP client).
- `sys::install`: program folder, version folders (0555/0555/0444), `current` link switched by one
  rename, records repaired from the link, retention of current + previous, link collision checks,
  uninstall. `sys::service`: systemd user unit / launchd agent (restart after 5 s).
- `cli::install`: `install --from`, `update [--from|--tag] [--restart]`, `rollback [--expect]
  [--restart]`, `status --install`, `check`, `uninstall`, `service enable|disable`, `version`,
  `extension`. Exit statuses: held 6, switched back 7, needs attention 8. `bridge install|status`
  are gone (clean cutover); tests link the bridge from `cataly extension`.
- Hub: `hub.stop` (refused while a session works or has a dialog), daily release notice task
  (`[update] check`, `first_after`, `api_base` loopback override; event `update.available` to the
  lead, once per version, never for `<program>/silenced`), usage counts (`[usage]`, off, no default
  collector, `DO_NOT_TRACK` wins, hourly OTLP/HTTP JSON with counts only, 2 s, key dropped on 401/403).
- Extension carries `VERSION`, filled in from the manifest by `cataly extension`; sent in the hello.
- `install.sh`, `.github/workflows/release.yml` (tag gate, CI via `workflow_call`, two builds per
  target compared with `cmp`, format/version checks, digest hand-over, signing with openssl, draft →
  upload missing → read back → publish, upgrade-from-previous step), `ci.yml` gains `workflow_call`.
- Home backup (decision 16): `[jobs.x] include_cataly = true` makes each live backup tick copy the
  config file to `config/cataly.toml` and write an online-backup snapshot of the store to
  `store/cataly.db` before the secret scan. `cataly setup --backup [--backup-repo owner/name]`
  reads the `gh` login (`gh api user`), defaults to `<login>/cataly-home`, reads the repository
  (`gh api repos/<o>/<n>`): private is used, public is refused, absent is only offered in the
  preview and created with `gh repo create --private` on `--apply --create` (then read back, fail
  closed). Applying makes the state folder a git tree (`git init`), adds the missing ignore lines
  (token and secret names, `*.pem`, `*.key`, `*.db` except `/store/cataly.db`, `*-wal`, `*-shm`,
  `*-journal`, temp, lock and socket files), sets `origin`, and writes `[jobs.home]` through the
  same snapshot/`--expect` path as the rest of setup.
- Restore (tested in `tests/owner.rs`): clone the backup, copy `store/cataly.db` to
  `<state>/cataly.db` with mode 0600 (the hub refuses group/other access), copy
  `config/cataly.toml` to the config file, start the hub; it opens the store and runs migrations.
- `rollback` writes the "do not announce again" mark only after the hub stopped (or none ran), so
  a held `--restart` leaves nothing behind.

## Signing key (owner)

`openssl genpkey -algorithm ed25519 -out cataly-release.pem`; repository secret
`CATALY_SIGNING_KEY` = the PEM; repository variable `CATALY_RELEASE_KEY` =
`openssl pkey -in cataly-release.pem -pubout -outform DER | tail -c 32 | od -An -tx1 -v | tr -d ' \n'`.
The release build compiles the public key in; a local build without it reads `CATALY_RELEASE_KEY`
at run time (tests use this with a throwaway key).

## Choices

- Slack MSG-14: a bot user's photo and name cannot be set with a bot token (users.setPhoto refuses
  bot tokens; the app icon is set in the app's settings), and a member account is human and is never
  changed (OPC req. 67). Not built; setup guidance should point to the app settings.
- Test versions above the crate's are real builds of a copy of the crate with the manifest version
  raised (`target/install-variants`, debug info off to stay under the 128 MiB bound).

## Not done (left)

- Onboarding OPC-66..73 (a draft module exists outside the tree, not committed), check 8.
- Slack workspace tag on destinations (SLACK-04).
- Local reproducible-build run (check 5 is CI-only: the workflow step is written).
