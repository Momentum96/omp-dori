# Contributing

Thanks for looking. This repository is a small, strict codebase; a change lands when it is one concern, it has evidence, and every gate is green.

## Upstream CLA process (sisyphuslabs/dori-superagent only)

The CLA process below belongs to upstream [sisyphuslabs/dori-superagent](https://github.com/sisyphuslabs/dori-superagent) and is kept here as attribution. This fork's CLA workflow is intentionally disabled: the CLA Assistant job in `.github/workflows/cla.yml` runs only when the repository is `sisyphuslabs/dori-superagent`, so it never runs for pull requests to this fork, and signing comments are not required for PRs to this fork. [CLA.md](CLA.md) is retained unchanged as the upstream legal text.

For reference, the upstream process is: every contributor signs the [Contributor License Agreement](CLA.md) once, before a pull request of theirs can be reviewed or merged there. Comment on the pull request with exactly:

```
I have read the CLA Document and I hereby sign the CLA
```

The CLA Assistant workflow then records the signature and the check turns green; it never asks again. Signing is what lets upstream keep one licence for every contribution, including the relicensing rights it needs when it changes its licence — the [licence](LICENSE) is the Sustainable Use License, and the CLA is what makes future licence changes possible without hunting down every author.

## Source development and public releases

Consumers follow the [verified release installation and support guide](docs/releases.md) and use `dori update check|install|status|rollback`. Maintainers work in a separate ordinary checkout; the source-only installer is `scripts/install-source.sh`. To install the current checkout beside an existing Dori without pulling, use `DORI_INSTALL_MODE=side DORI_NO_PULL=1 DORI_SRC="$PWD" bash scripts/install-source.sh`. `OMP_DORI_HOME` selects the side config/state/wrapper home (default `~/.omp-dori`); `OMP_DORI_BIN_DIR` selects the `omp-dori` command destination. The wrappers and `omp-dori-mode` skill keep lanes in that home, with `HC_SESSION=omp-dori` by default (`OMP_DORI_HC_SESSION` overrides it). See the [source installation guide](docs/releases.md#source-checkouts-for-maintainers) for modes, backups and path overrides.

Consumer control metadata and the managed source pointer protect the selected installation home in both modes, including aliases: `primary` source installation holds on consumer-managed `~/.dori`, and `side` requires a separate destination. An unmanaged source home retains the existing primary workflow; another unmanaged Dori prompts in a terminal or holds without one unless a mode is explicit. Do not develop inside a frozen installed release. Source fixture checks, real OMP wrapper/skill isolation and provider or messenger QA are separate evidence; this change claims no new live QA.

Upstream porting and candidate/recovery operations use `dori upstream ... --config FILE`. That interface can use configured models and maintainer publication permissions; consumer release management uses neither. Build fixed public assets with `bun scripts/release.ts build --out DIST --prerelease`, then follow the [exact-commit review/QA/CI/tag publication procedure](docs/releases.md#building-and-publishing-releases). Source checks, real runtime/messenger evidence and independent external-user testing are distinct claims.

## Run the gates

```sh
bun install --frozen-lockfile
(cd skills/dori-mode/scripts && bun install --frozen-lockfile)
(cd extension && bun install --frozen-lockfile)
bun run ci
```

`bun run ci` is the same chain CI runs:

- `check` — Biome lint and format.
- `typecheck` — `tsc --noEmit` for the harness, skill scripts and extension.
- `knip` — no unused files, exports or dependencies.
- `lint:loc` — the TypeScript file-length gate, with a 250-line limit per capped file.
- `test` — the harness suite.
- `test:skill` — the skill's script suite.
- `test:extension` — the OMP extension suite.

The unit suites need `bun` and `git` and use recorded stand-ins for `herdr` and `gh`. `test/e2e.test.ts` starts a real `herdr` server and is skipped when `herdr` is not on `PATH`.

The extension's `with the real omp-ulw` group executes `ref/omp-ulw/index.ts` and skips when that file is absent. For real-fixture coverage, prepare the checkout exactly as the standard CI `check` and `test-macos` jobs do:

```sh
pin="$(tr -d '[:space:]' < omp-ulw.lock)"
git clone --quiet https://github.com/dbc-hbin/omp-ulw.git ref/omp-ulw
git -C ref/omp-ulw -c advice.detachedHead=false checkout --quiet "$pin"
test "$(git -C ref/omp-ulw rev-parse HEAD)" = "$pin"
bun run test:extension
```

Fixture preparation needs network access unless an exact local checkout is already available. Confirm that the real-fixture tests ran without being skipped: both role gating and the current `⁂` prompt-marker regression must execute. A fixture-free run covers the local FakePI seam but supplies no runtime proof that the pinned dependency replaces `HOST_ROLE` with an `<omp-model-preset>` result. Git commit metadata alone also supplies no such proof. This fixture test does not run an OMP provider or establish live session, messenger or container compatibility.

## A pull request

- One concern per pull request, with a title that says what changed and a body that says why.
- Evidence, not adjectives: the command you ran and what it printed, or the file and line you read. A claim you did not verify is written as unverified.
- Tests where behaviour changed; a test that cannot fail is not a test.
- Public surfaces — issues, pull requests, commits, review comments — are English.
- Do not edit another person's work to make your change pass; report it instead.

## Licence

The project is licensed under the [Sustainable Use License](LICENSE), with third-party components under their own licences. By contributing you agree your contribution is licensed under the same terms. The upstream [CLA](CLA.md) is retained unchanged and applies to upstream sisyphuslabs/dori-superagent only; this fork does not require a CLA signature.
