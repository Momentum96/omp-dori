import { expect, test } from "bun:test";
import { existsSync, mkdirSync, readFileSync, readlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { git, setup } from "./install-setup.ts";

// A local omp-ulw remote with one commit per call, and omp-dori's lock pointing at its first commit.
function withUlwRemote() {
  const s = setup();
  const remote = join(s.root, "ulw-remote");
  mkdirSync(remote);
  git(remote, "init", "-q", "-b", "main");
  const commit = (text: string): string => {
    writeFileSync(join(remote, "index.ts"), `export default () => {}; // ${text}\n`);
    git(remote, "add", ".");
    git(remote, "commit", "-qm", text);
    return git(remote, "rev-parse", "HEAD");
  };
  const first = commit("u1");
  writeFileSync(join(s.upstream, "omp-ulw.lock"), `${first}\n`);
  git(s.upstream, "add", "omp-ulw.lock");
  git(s.upstream, "commit", "-qm", "lock");
  const config = () => JSON.parse(readFileSync(join(s.home, ".dori/config.json"), "utf8"));
  const ulwHead = () => git(join(s.home, ".dori/omp-ulw"), "rev-parse", "HEAD");
  const install = (extra: Record<string, string> = {}) =>
    s.install({ OMP_ULW_SRC: "", OMP_ULW_REPO: remote, ...extra });
  return { ...s, first, commit, config, ulwHead, install };
}

test("by default omp-ulw follows its latest main, not the lock", () => {
  // Given an omp-ulw remote whose main is one commit past the lock.
  const { home, commit, config, ulwHead, install } = withUlwRemote();
  const latest = commit("u2");
  // When install.sh runs; then the checkout is at main, linked as an OMP extension and not imported by omp-dori.
  expect(install().code).toBe(0);
  expect(ulwHead()).toBe(latest);
  expect(readlinkSync(join(home, "extensions/omp-ulw"))).toBe(join(home, ".dori/omp-ulw"));
  expect(config().ompUlwPath).toBe("");
});

test("a later install moves omp-ulw to the newer main", () => {
  // Given an installed omp-ulw and a newer omp-ulw commit.
  const { commit, ulwHead, install } = withUlwRemote();
  expect(install().code).toBe(0);
  const newer = commit("u2");
  // When install.sh runs again; then omp-ulw is at the newer commit.
  expect(install().code).toBe(0);
  expect(ulwHead()).toBe(newer);
});

test("OMP_ULW_REF=lock keeps the revision in omp-ulw.lock", () => {
  // Given an omp-ulw remote whose main is past the lock.
  const { first, commit, ulwHead, install } = withUlwRemote();
  commit("u2");
  // When install.sh runs with OMP_ULW_REF=lock; then omp-ulw is at the locked commit.
  expect(install({ OMP_ULW_REF: "lock" }).code).toBe(0);
  expect(ulwHead()).toBe(first);
});

test("a standalone omp-ulw extension is reused: no second checkout and no second import", () => {
  // Given an omp-ulw package already installed as an OMP extension.
  const { home, config, install } = withUlwRemote();
  const standalone = join(home, "extensions/omp-ulw");
  mkdirSync(standalone, { recursive: true });
  writeFileSync(join(standalone, "package.json"), '{ "name": "omp-ulw" }\n');
  // When install.sh runs.
  const r = install();
  // Then omp-dori clones nothing and its config leaves omp-ulw to that extension.
  expect(r.code).toBe(0);
  expect(r.err).toContain(`standalone extension at ${standalone}`);
  expect(existsSync(join(home, ".dori/omp-ulw"))).toBe(false);
  expect(config().ompUlwPath).toBe("");
});
