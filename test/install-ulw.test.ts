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

test("an OMP older than omp-ulw main needs gets the lock revision unless OMP_ULW_REF says otherwise", () => {
  // Given an omp-ulw remote whose main is past the lock, and OMP 18.18.9.
  const { first, commit, ulwHead, install } = withUlwRemote();
  const latest = commit("u2");
  // When install.sh runs without OMP_ULW_REF; then omp-ulw stays at the lock and the reason is printed.
  const r = install({ INSTALL_TEST_OMP_VERSION: "18.18.9" });
  expect(r.code).toBe(0);
  expect(r.err).toContain("omp-ulw.lock is used");
  expect(ulwHead()).toBe(first);
  // And an explicit OMP_ULW_REF=main is followed on that OMP.
  expect(install({ INSTALL_TEST_OMP_VERSION: "18.18.9", OMP_ULW_REF: "main" }).code).toBe(0);
  expect(ulwHead()).toBe(latest);
});

const ulwCheckout = (dir: string) => {
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "index.ts"), "export default () => {};\n");
  writeFileSync(join(dir, "presets.ts"), "export {};\n");
};

test("a standalone omp-ulw extension is reused: no second checkout and no second import", () => {
  // Given omp-ulw installed as an OMP extension by hand, at a revision from before it had a package.json.
  const { home, config, install } = withUlwRemote();
  const standalone = join(home, "extensions/omp-ulw");
  ulwCheckout(standalone);
  // When install.sh runs.
  const r = install();
  // Then omp-dori clones nothing and its config leaves omp-ulw to that extension.
  expect(r.code).toBe(0);
  expect(r.err).toContain(`standalone extension at ${standalone}`);
  expect(existsSync(join(home, ".dori/omp-ulw"))).toBe(false);
  expect(config().ompUlwPath).toBe("");
});

test("a config that points ompUlwPath at its own checkout keeps that checkout, now as the extension", () => {
  // Given a config whose ompUlwPath is a checkout of the owner's own.
  const { home, config, install } = withUlwRemote();
  const own = join(home, "my-ulw");
  ulwCheckout(own);
  mkdirSync(join(home, ".dori"));
  writeFileSync(join(home, ".dori/.omp-dori"), "primary\n");
  writeFileSync(join(home, ".dori/config.json"), JSON.stringify({ ompUlwPath: join(own, "index.ts") }));
  // When install.sh runs; then nothing is cloned, that checkout is the extension, and the import path is emptied.
  const r = install();
  expect(r.code).toBe(0);
  expect(r.err).toContain(`config.json points at ${own}`);
  expect(existsSync(join(home, ".dori/omp-ulw"))).toBe(false);
  expect(readlinkSync(join(home, "extensions/omp-ulw"))).toBe(own);
  expect(config().ompUlwPath).toBe("");
});

test("a standalone omp-ulw added after an install replaces the installer's own link", () => {
  // Given an install that linked its omp-ulw checkout, and a standalone omp-ulw added later.
  const { home, install } = withUlwRemote();
  expect(install().code).toBe(0);
  expect(readlinkSync(join(home, "extensions/omp-ulw"))).toBe(join(home, ".dori/omp-ulw"));
  ulwCheckout(join(home, "extensions/omp-ulw-main"));
  // When install.sh runs again; then the standalone is used and the earlier link is gone, so omp-ulw loads once.
  const r = install();
  expect(r.code).toBe(0);
  expect(r.err).toContain("standalone extension at " + join(home, "extensions/omp-ulw-main"));
  expect(existsSync(join(home, "extensions/omp-ulw"))).toBe(false);
});
