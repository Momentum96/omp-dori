# Installing, updating and releasing: behaviour spec

## Purpose

How the product reaches a user's machine and stays current. Three audiences:

- The **owner** installs the product once, checks for and installs new versions, and rolls back a bad one.
- The **maintainer** cuts a release from a tagged commit and publishes it on GitHub.
- **CI** proves, for every change and every release, that the product builds the same bytes twice, works against
  the OMP releases it claims to support, and installs, updates and rolls back cleanly on both release targets.

The new product is one Rust binary (the daemon and its command line) plus one small TypeScript extension file that
OMP loads. A release therefore carries two files per target, not a source tree. Release targets are linux-x64 and
darwin-arm64 only. There is no patched OMP, no bundled script runtime and no dependency cache: OMP is installed and
updated by the owner with OMP's own tools.

Installing never starts anything. It does not start the daemon, write configuration, enable a service, touch OMP's
configuration or executable, or contact a messenger. Configuration writing belongs to the setup capability in the
coordination area (COORD-51); session start and health belong to the session-host area (HOST-03, HOST-35); the
extension's behaviour once loaded belongs to the omp-extension area (EXT-01).

## Capabilities

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| IR-01 | Bootstrap script that fetches a release (latest stable, exact tag or local folder), verifies it and hands over to the installer | owner | SIMPLIFY | Script only downloads and checks hashes, then runs the verified binary's own installer; one install implementation. |
| IR-02 | Release description listing every source file with digest and mode, source hash, archive digest, required OMP version, checked against tag, commit and tree through the GitHub API | installer | SIMPLIFY | Two files per target need only a checksum list; version facts come from the binary itself. |
| IR-03 | Hand-written strict tar reader that admits only listed regular files | installer | DROP | No archive is shipped; raw assets need no extractor. |
| IR-04 | File inside the release declaring which hosts and runtimes it supports | installer | DROP | The presence of a per-target asset says it. |
| IR-05 | Install-time checks for a script runtime of a minimum version and for git | installer | DROP | A single binary needs neither. |
| IR-06 | Two kinds of installation home (main and isolated), different per-OS defaults, home overrides by flag and variable | owner | SIMPLIFY | One per-user location on both systems; one override for tests. |
| IR-07 | Retained read-only release folders named by commit and hash, selected by one active link | installer, daemon | SIMPLIFY | Keep "versions side by side, one atomic link"; folders hold two files. |
| IR-08 | Generated wrapper scripts, a per-home OMP settings overlay, a renamed copy of the skill per release, stored hashes for each generated file | owner | DROP | Existed to steer a patched OMP and keep two installs apart; the daemon launches stock OMP itself. |
| IR-09 | Create an example configuration if absent, preview configuration changes, read effective settings back | owner | MERGE into COORD-51 | Setup is its own command, never part of install. |
| IR-10 | Patched OMP toolchain: pinned upstream commit, patch file, private clone, frozen native add-on, identity records for runtime and launcher, a checking launcher | sessions | DROP | Principle 1: stock OMP only, no patch. |
| IR-11 | Toolchain cache with per-process leases, an exclusive prune lock, quarantine renames, grace-period deletion, process scans and trust flags | owner | DROP | Only existed because the patched toolchain was large and shared; nothing is cached now. |
| IR-12 | Install transaction: exclusive lock, before/after record, pointer switch, recovery on next run, distinct outcome classes | installer | SIMPLIFY | A single atomic link rename needs no recovery record; keep the lock and the outcome classes. |
| IR-13 | Refuse to switch while anything of the product runs, judged by reading many lock, record and runtime files and probing their process ids | installer | SIMPLIFY | Ask the daemon over its socket; it is the only long-lived process. |
| IR-14 | Per-launch leases written by every OMP start and a short gate lock around the switch | installer | DROP | Version skew is caught by the extension handshake instead. |
| IR-15 | Re-hash every retained file at status, rollback and each OMP launch | installer | SIMPLIFY | Hash the two files of a version before switching to it and at status. |
| IR-16 | Roll back to the single retained previous version, optionally asserting which version that is | owner | KEEP | Cheap, essential recovery path. |
| IR-17 | Status: installed and previous versions, reconciliation of an interrupted switch | owner | KEEP | Needed; reconciliation disappears with IR-12. |
| IR-18 | Check: report the available version against the installed one without changing anything | owner | KEEP | Needed for the daily notice and for scripts. |
| IR-19 | Install from a local folder of release assets | owner, CI | KEEP | Required by CI and by offline or pre-downloaded installs. |
| IR-20 | Stable releases by default, prereleases only by exact tag, never downgrade except by rollback | owner | KEEP | Safe default. |
| IR-21 | Automatic update attempted at most daily when OMP is launched, with a 15 s network budget, installing a newer stable release unless something runs | owner | SIMPLIFY | Daemon checks daily and tells the owner; installing stays an explicit act (see questions). |
| IR-22 | Automatic-update on/off/status, a per-launch pause variable, rollback pausing further automatic installs | owner | SIMPLIFY | One setting for the daily notice; rollback silences the rejected version. |
| IR-23 | Make the extension discoverable through a link in OMP's per-user extension folder | OMP | KEEP | The documented way OMP finds extensions (EXT-01). |
| IR-24 | Search order for the native binary across a variable, the code tree, a deployment build folder and the home | daemon, extension | SIMPLIFY | One fixed location through the active link. |
| IR-25 | Guard against OMP's own updater replacing a wrapper that stood in for OMP | owner | DROP | Nothing wraps OMP any more. |
| IR-26 | Source-checkout installer with two modes, pulls, builds, backups and takeover of foreign links | maintainer | DROP | A maintainer builds release files locally and installs them with IR-19. |
| IR-27 | Managed live deployment from CI-green main: idle and owner-quiet detection, stop, swap, restart, health read-back, rollback, an external supervisor service | owner | SIMPLIFY | Becomes one explicit "install and restart" path with a health check and automatic switch-back, plus an optional user service. |
| IR-28 | Tool that ports changes from the upstream project | maintainer | DROP | Owner decision; summary under Problems. |
| IR-29 | Daily workflow that prepares a new patched OMP bundle and opens a pull request or issue | maintainer | DROP | No patch exists; replaced by the nightly run against the latest OMP (IR-38). |
| IR-30 | Release built only from committed bytes; product version must equal the tag | maintainer | KEEP | Prevents shipping uncommitted work. |
| IR-31 | Reproducible native build: pinned compiler, no user build settings, path remapping, fixed timestamps, two independent builds compared byte for byte, executable format checked | maintainer | KEEP | Core supply-chain proof. |
| IR-32 | macOS binary built on a macOS runner, handed to the Linux publisher with its digest, re-checked there | maintainer | KEEP | Each target builds natively; the digest binds the hand-over. |
| IR-33 | Deterministic source archive plus a filter that keeps secrets and private folders out of it | maintainer | DROP | No source is shipped in a release. |
| IR-34 | Publish gate: tag resolves to the commit, commit is on main, the separate CI workflow passed for that exact commit and app, first release must be a prerelease | maintainer | SIMPLIFY | The release workflow runs the full CI itself on the tag commit; only the main-ancestry check remains. |
| IR-35 | Draft-then-publish with an on-disk intent record, markers in the release body, read-back of every asset and no replay of ambiguous writes | maintainer | SIMPLIFY | GitHub's own release state is the record; read before every write, never overwrite. |
| IR-36 | Main CI: lint, format, type check, a per-file line cap, dead-code scan, three TypeScript test suites | maintainer | SIMPLIFY | Rust checks plus the extension's checks; one suite per language. |
| IR-37 | CI that prepares the patched OMP and runs its tests | maintainer | DROP | No patch. |
| IR-38 | CI against official OMP release binaries: pinned versions on every change, the latest release nightly, offline with a loopback model | maintainer | KEEP | The proof that the product stays thin over stock OMP. |
| IR-39 | Cross-OS CI: macOS full suites and a release-install check, Windows script suites | maintainer | SIMPLIFY | macOS arm64 is a release target and gets the same jobs as Linux. |
| IR-40 | End-to-end install, reinstall, start and status check of a locally built release in an isolated home | maintainer | KEEP | Extend with update, rollback, tamper and running-daemon cases. |
| IR-41 | Contributor licence agreement workflow | maintainer | DROP | Belongs to the upstream project's repository. |
| IR-42 | Local developer setup that mirrors CI and downloads a pinned script runtime | maintainer | DROP | Cargo and the extension's type checker are enough. |
| IR-43 | No migration from the predecessor product; install fresh | owner | KEEP | Owner decision: no old names, paths or readers. |

## Requirements

### A. What a release contains (IR-02, IR-04, IR-30)

1. A release is a GitHub release on the product's canonical repository, tagged `v` followed by the product version
   (`MAJOR.MINOR.PATCH`). A tag with a semver pre-release suffix (for example `-rc.1`) is published as a GitHub
   prerelease; any other tag as a normal release.
2. Release assets, all named with the version: one executable per target (linux-x64, darwin-arm64), one extension
   file, one install script, and one checksum list. Nothing else.
3. The checksum list has one line per other asset: lowercase hex SHA-256, two spaces, asset name, in name order,
   in the format `sha256sum --check` and `shasum -a 256 --check` accept.
4. The extension file is a single self-contained TypeScript file importing only OMP's public extension API. It is the
   same file for both targets.
5. The binary reports, on a machine-readable version command that needs no configuration and starts nothing: product
   version, commit, target, minimum supported OMP version, and extension protocol version. The extension file carries
   the same product version and protocol version as constants.
6. The version in the Rust package manifest is the single source of the product version. The extension's version
   constant is generated from it at build time, never edited by hand.

### B. First install (IR-01, IR-05, IR-06, IR-19, IR-20, IR-43)

7. The install script needs only a POSIX shell, `curl`, `uname`, and `sha256sum` or `shasum`. It refuses any host
   other than Linux x86_64 or macOS arm64 and names the supported pair.
8. Release selection: no argument means the latest stable release (GitHub's "latest release", which excludes
   prereleases and drafts); an exact tag selects that release including prereleases; a local folder selects the
   assets in it. If no stable release exists yet, the script says so and suggests an exact tag.
9. Downloads use HTTPS only, follow redirects only to HTTPS, and have bounds: 10 s to connect, 120 s per asset,
   checksum list at most 64 KiB, extension at most 1 MiB, binary at most 128 MiB. Exceeding any bound fails the
   install with the asset named.
10. The script downloads the checksum list, the binary for this host and the extension file into a fresh private
    temporary folder, checks both hashes against the list (exactly one matching line each), and stops on any mismatch
    or missing line without running anything. It then runs the verified binary's install command, pointing it at the
    temporary folder as a local asset source, and removes the temporary folder whatever the outcome.
11. The binary's install command repeats the hash checks itself (requirement 23), so the script's checks are a
    convenience, not the only line of defence.
12. Installation is fresh. Nothing reads, imports or migrates files of the predecessor product. Release notes tell
    predecessor users to stop it and copy settings by hand.

### C. Where files go (IR-06, IR-07, IR-24, IR-23)

13. One program folder per user: `$XDG_DATA_HOME/cataly` when that variable is set and absolute, otherwise
    `~/.local/share/cataly`, on both Linux and macOS. An explicit absolute path given to the install command
    overrides it (used by CI and tests); there is no second kind of installation.
14. Inside it: one folder per installed version holding exactly the binary and the extension file; a link naming the
    active version folder; a small record per version (requirement 18); a lock file; a staging folder.
15. Version folders and their files are made read-only after verification (folder 0555, binary 0555, extension 0444).
    The program folder is 0700 and must be owned by the user and not group- or world-writable; otherwise install
    refuses and names the path.
16. The command on the user's path is a link at `~/.local/bin/cataly` pointing to the binary through the active
    link, so switching versions needs no change to it. If `~/.local/bin` is not on `PATH`, install says so once and
    changes no shell profile.
17. The extension is made visible to OMP by one link, named after the product with a `.ts` suffix, in OMP's per-user
    extension folder (`~/.omp/agent/extensions/`, or the folder under OMP's agent-directory override when that is
    set), pointing to the extension file through the active link. Switching versions therefore switches the
    extension in the same rename. If the folder is missing, install creates it (mode 0700) and nothing else of OMP's.
    OMP sessions already running keep the extension they loaded; new sessions get the new one.
18. Per-version record (survives restart): version, commit, target, SHA-256 of the binary and of the extension,
    where it came from (release URL or local folder), install time, and whether it is current or previous. It holds no
    secrets.
19. The daemon and the extension find each other only through the daemon socket (session-host rule 71); neither
    searches for the other's files. The binary path the daemon uses for helpers is its own executable path.
    Version skew (an OMP session started before a switch, talking to a daemon started after it) is caught by the
    extension's hello (EXT-01, requirement 3 there): a different extension protocol version gets role "none" plus
    one owner notice naming the session and asking for it to be restarted. Equal protocol versions work together.
20. Install never creates configuration, state, a service, or any file outside the program folder, the command link
    and the extension link.
21. Collisions: if the command link or the extension link path exists and is not a link into this program folder,
    install refuses, names the path, and changes nothing. It never deletes, renames or overwrites a file it did not
    create.

### D. Install, update, rollback, status (IR-12, IR-13, IR-15 to IR-18, IR-20)

22. One operation at a time: install, update, rollback and uninstall take an exclusive OS file lock on the lock file
    without waiting. A second operation fails at once with "another install operation is running".
23. Before using any asset the binary checks: the checksum list has exactly one line for each needed asset; the
    SHA-256 of the binary and the extension match; the binary is an executable for this host (ELF 64-bit x86-64 on
    Linux, Mach-O 64-bit arm64 on macOS); the candidate binary's version report (requirement 5, run with a 5 s limit
    and 64 KiB output cap) states the expected version and this target.
24. Staging: assets are written into a fresh folder under the staging folder (same file system as the version
    folders), hashed while writing, then renamed to the version folder. Any staging folder found while holding the
    lock is a leftover and is deleted first.
25. OMP compatibility: if `omp --version` (5 s limit) reports a version below the candidate's minimum, a first install
    proceeds with a warning; an update or rollback is held and tells the owner to update OMP first with OMP's own
    updater. If OMP is absent, install warns and proceeds.
26. Daemon running: the operation connects to the daemon socket (1 s). If a daemon answers, an update or rollback is
    held: nothing is switched, and the message says to stop the daemon or use the restart option (requirement 33).
    A first install cannot meet a running daemon of this product and does not check.
27. Switch: create a new link with a temporary name pointing to the target version folder, rename it over the
    active link, then flush the program folder to disk. This one rename is the whole activation; the command and
    extension links never change after first install.
28. Records are updated after the rename: the new version is marked current, the old current becomes previous. If
    the process dies between rename and record update, status (requirement 31) repairs the records from the link,
    which is the single source of truth for "current".
29. Retention: after a successful switch, version folders that are neither current nor previous are deleted. This is
    safe without leases because Unix keeps a deleted file alive for any process that still has it open or mapped.
30. Rollback switches to the previous version after re-checking its two hashes against its record, and makes the
    version rolled back from the new previous. An optional expected version must match the previous version or the
    rollback is held. Rollback when there is no previous version is refused.
31. Status prints current and previous version, commit, source and install time, checks that the active link,
    command link and extension link point where they should, and re-hashes the current version's two files. Any
    disagreement is reported with the path and the exact repair command; status itself changes only the records
    (requirement 28).
32. Check reports the latest stable release (or the given tag) against the current version: newer, same, or older.
    It downloads only the GitHub release metadata (at most 1 MiB, 15 s total) and changes nothing.
33. Install and restart (IR-27): an explicit option on update and rollback for a running daemon. Steps: ask the
    daemon for orderly shutdown (session-host area defines it) and wait up to 60 s for the socket to close; switch;
    start the new daemon the same way it was started (the user service if enabled, otherwise detached); wait up to
    30 s for its socket to answer a status request reporting the new version. On timeout or wrong version: stop it,
    switch back, start the old version, report "switched back". If that also fails, report "needs attention" with
    both versions named.
34. Outcomes are distinct exit statuses: done; held (nothing changed, reason given); switched back (the system is as
    before); needs attention (state differs from both before and after). Messages never contain secrets.
35. Uninstall removes the command link, the extension link and the program folder, and stops a user service it
    created. It never touches configuration, state, sessions or OMP. It is held while the daemon runs.

### E. Update notice (IR-21, IR-22)

36. The daemon checks for a newer stable release once per 24 h, the first time 10 min after start. The check is one
    request for the latest release metadata with a 15 s total limit. The next check is due 24 h after the attempt,
    whatever its result, so offline machines never retry in a loop.
37. A strictly newer stable version (compared numerically by major, minor, patch) produces one owner-visible notice
    through the daemon's normal notice path (watch-schedule area), stating the version and the exact update command.
    The same version is announced once; a version the owner rolled back from is never announced again.
38. The daemon never installs, restarts or downloads binaries on its own. One setting turns the check off; while off
    no network request is made.
39. Last check time, last seen version and the silenced version survive a restart.

### F. Optional user service (IR-27)

40. An explicit command writes a systemd user unit (Linux) or a launchd agent in `~/Library/LaunchAgents` (macOS)
    that runs the daemon through the command link, restarting on failure with a 5 s delay, and enables it. A matching
    command disables and removes it. Install and update never do either. Restarting the service after an update is the
    owner's choice or the restart option's job.

### G. Building and publishing a release (IR-30 to IR-35)

41. A release is produced only by the release workflow, started by pushing a version tag. The tag must equal `v`
    plus the package version, and the tagged commit must be reachable from main; otherwise the workflow stops before
    building.
42. The workflow first runs the complete CI (section H) on the tagged commit, on both targets. Publishing depends on
    all of it passing; no result from another workflow run is consulted.
43. Each target is built on a native runner of that target (Ubuntu x64, macOS arm64) from a clean checkout of the
    commit: pinned compiler version from the repository, locked dependencies, no user or runner build configuration,
    build paths remapped to a fixed prefix, fixed timestamp, incremental compilation off, single codegen unit.
44. Each runner builds twice in two separate fresh folders and compares the binaries byte for byte. A difference
    fails the release. The runner also checks the executable format (requirement 23) and runs the version report.
45. Each build job passes its binary to the publish job together with its SHA-256 as a job output; the publish job
    recomputes the hash and stops on mismatch.
46. The publish job generates the extension file and the checksum list, then: if no release exists for the tag,
    creates a draft; uploads each missing asset with its exact byte length; downloads every asset back and compares
    hashes; then marks the release published (prerelease if the tag says so).
47. Re-running is safe: an existing draft with matching assets is continued; an existing asset with a different hash
    stops the workflow and is never replaced; an already published release is only read back and verified. GitHub's
    release state is the only record; nothing is kept on the runner.
48. Workflow permissions are read-only except the publish job, which may write releases. Third-party actions are
    pinned by commit hash.

### H. What CI must prove (IR-36, IR-38 to IR-40)

49. On every pull request and push to main, on both linux-x64 and darwin-arm64: formatting check, lint with warnings
    as errors, all Rust tests with locked dependencies, the extension's type check and tests against OMP's public
    extension types, and a release build. Each job has a time limit (30 min).
50. Stock OMP compatibility on every pull request: for the minimum supported OMP version and the newest version known
    at the time of the change, download the official OMP release binary for the runner's target, verify it against
    the checksum file published with that OMP release, and check that `omp --version` names the expected version.
51. With that OMP, in an isolated home with no network model and a loopback fake model provider: run the health check
    (HOST-35); start a lead session; deliver a message and receive the scripted reply; resume the session by its file
    and confirm the same session id; confirm a wrong expected id is refused; confirm the extension was discovered
    through the extension link and completed its hello with the daemon.
52. Nightly, the same compatibility job runs against the latest published OMP release resolved at run time. A
    failure shows on the repository and notifies the owner through GitHub's normal workflow-failure notification.
53. Install path, on both targets, against locally built assets in an isolated home: first install through the
    install script from a local folder; layout, modes and both links as in section C; reinstall of the same version
    is a no-op; update to a second build with a higher version; rollback; status clean after each step.
54. Failure cases that must be proven: a tampered binary, a tampered extension, a missing checksum line and a wrong
    target binary are each refused with nothing changed; a foreign file at the command or extension link path is
    refused; an update while a test daemon listens is held; the restart option switches back when the new daemon does
    not report healthy within its limit; an interrupted switch (killed between rename and record) is repaired by
    status.
55. Upgrade from the previous published release: install the last published release, run its setup with a fixture
    configuration, update to the candidate, and confirm the daemon starts and reads that configuration and state.

## Third-party interfaces used

- GitHub REST API: latest release, release by tag, release assets (anonymous reads for installs and checks);
  create release (draft), upload asset with exact `Content-Length`, update release (publish) for the publisher.
- GitHub Actions: tag-push trigger, scheduled trigger, `workflow_call` for reusing CI, job outputs, artifacts,
  `ubuntu` x64 and `macos` arm64 hosted runners, pinned actions.
- OMP: `omp --version` output `omp/X.Y.Z`; official release assets per platform (for example `omp-linux-x64`) with
  a published `SHA256SUMS.txt`; per-user extension folder `~/.omp/agent/extensions/`; `-e`, `--no-extensions`,
  `--mode rpc-ui`, `--offline`; the agent-directory override variable `PI_CODING_AGENT_DIR` used for isolated tests
  (confirm it is documented); OMP's own `omp update` for updating OMP.
- Rust toolchain: `rustup` toolchain file, `cargo build --locked`, `--remap-path-prefix`, `SOURCE_DATE_EPOCH`.
- Coreutils `sha256sum`, macOS `shasum`, `curl`; systemd user units; launchd user agents; XDG base directories.

## State that must survive a restart

- Which version is current (the active link) and which is previous (version records).
- Per version: version, commit, target, hashes of both files, source, install time.
- Update notice: time of last check, last version announced, version silenced by rollback, the on/off setting.
- Whether a user service was created by the product (so uninstall removes only its own).

## Problems in the old approach, and better ideas

- **A patched OMP rebuilt on every machine.** Each install cloned a pinned upstream OMP commit, applied a large
  patch, verified the resulting tree, installed a frozen native add-on and stored identity records for the add-on,
  the script runtime and the launcher. Better: stock OMP only; Cataly states a minimum OMP version and probes features.
- **Machinery that existed only to manage that toolchain** (all DROP):

  | Old machinery | Why it existed |
  | --- | --- |
  | Per-user toolchain cache keyed by full source identity | Rebuilding the patched OMP was slow and large. |
  | Leases per process (pid, start time, boot id) | Stop a cache cleanup from deleting a copy a running session used. |
  | Exclusive cleanup lock with a five-minute wait | Serialise cleanup against writers taking leases. |
  | Quarantine renames, grace period, committed-removal renames | Make deletion reversible because older writers took no leases. |
  | Process scans of working folders, open files, mappings, `lsof` on macOS, trust flags | Prove no process used a copy before deleting it. |
  | Bound-source lock, patch file and post-image digests | Prove the patched OMP was exactly the reviewed one. |
  | Identity records for the script runtime, add-on and launcher | Detect drift in the script runtime that ran OMP. |
  | Per-launch leases and a launch gate | Stop a version switch while an OMP launch was choosing its code. |
  | Release capability file | Declare which hosts a source release ran on. |
  | Full public source listing in the release description | Releases shipped source, executed from source. |
  | Hand-written tar reader and secret-path filter | Safely unpack a source archive. |
  | Wrapper scripts, OMP settings overlay, renamed skill copies, stored hashes for each | Keep a main and an isolated install from mixing versions while wrapping OMP. |
  | Install transaction record and three-way recovery | Two links (source and OMP wrapper) had to move together. |
  | Daily patched-OMP candidate workflow | Keep the patch current with upstream OMP. |

  Unix semantics make most of this unnecessary for two small files: an unlinked binary stays valid for running
  processes, and one link rename is atomic.
- **Many processes and file probing to decide "is anything running".** The old installer read session-role files,
  controller locks, runtime and monitor records and session reservations, then probed process ids. Better: one
  daemon, one socket; ask it.
- **Two install implementations** (a large inline script in the bootstrap and the installer proper) duplicated
  verification. Better: the script only downloads and checks hashes; the binary installs.
- **Automatic installs at OMP launch** added a network check and possible switch to the start of OMP sessions, gated
  by leases. Better: the daemon checks daily off the critical path and notifies; the owner installs.
- **Cross-workflow CI proof.** The release looked up another workflow's result for the exact commit and app. Better:
  the release workflow runs CI itself on the tag commit.
- **On-disk publication intent record.** GitHub's release state already answers "what was done"; read it.
- **Managed deployment from main** followed CI-green main with idle detection, owner-quiet windows, an external
  supervisor and a multi-stage hand-over log. Better: releases only, an explicit restart option with a health
  check and switch-back, and an optional user service.
- **The upstream-porting updater (dropped by owner), in brief:** it watched the upstream project for new commits,
  ran a model "worker" in a locked-down OMP session to port each change into a candidate tree, ran checks and a
  separate model "reviewer" on the result, and recorded a hash-bound verdict. Ready candidates could be published as
  a pull request and promoted into the live installation with stop, swap, health check and rollback. It kept
  snapshots, recovery acknowledgements for orphaned model children, and an external supervisor service.
- **Linux portability.** The old Linux binary linked the system C library of the build runner. Better: a fully static
  binary (musl target) so one file runs on any x86-64 Linux; confirm with the owner.
- **No authenticity beyond the download host.** Checksums only prove the files match each other. See questions.

## Questions for the owner

1. Release authenticity: sign the checksum list (for example an ed25519 key whose public half is compiled into the
   binary and printed in the install script), use GitHub's build-provenance attestations (verification needs `gh`),
   or rely on HTTPS from GitHub plus checksums as today?
2. Updates: notice only (this spec), or may the daemon install a newer stable release by itself when no session is
   busy, using the restart option?
3. Update while the daemon runs: is the explicit "install and restart" option wanted, and may it stop busy sessions
   after the 60 s shutdown limit, or must it hold until every session is idle?
4. Linux binary: fully static (musl), or linked to the system C library with a stated minimum version?
5. Program folder: `~/.local/share/cataly` on macOS too (one rule), or `~/Library/Application Support` there?
6. Windows: no release and no CI for the Rust daemon (consistent with session-host), or should CI at least build it?
7. Does the product still ship an OMP skill next to the extension? If so it is a third file per release and needs a
   link in OMP's skill folder, installed the same way as the extension.
8. macOS: is notarisation or a Developer ID signature wanted? Files fetched with `curl` carry no quarantine flag, so
   the install script works without it, but a browser download would be blocked.
