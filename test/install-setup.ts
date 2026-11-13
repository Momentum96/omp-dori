import { afterEach } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// The real source installer over a local upstream; package installs and toolchain probes stay isolated.
export const script = resolve(import.meta.dir, "../scripts/install-source.sh");
const gitEnv = {
  ...process.env,
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_AUTHOR_NAME: "t",
  GIT_AUTHOR_EMAIL: "t@example.com",
  GIT_COMMITTER_NAME: "t",
  GIT_COMMITTER_EMAIL: "t@example.com",
};
const roots: string[] = [];
afterEach(() => {
  for (const r of roots.splice(0)) rmSync(r, { recursive: true, force: true });
});

export const git = (cwd: string, ...argv: string[]): string => {
  const r = Bun.spawnSync(["git", ...argv], { cwd, env: gitEnv });
  if (r.exitCode !== 0) throw new Error(`git ${argv.join(" ")}: ${r.stderr}`);
  return r.stdout.toString().trim();
};

const skillFixture = "---\nname: dori-mode\ndescription: Turns a coding agent into a Dori.\n---\n\n# Dori mode\n";

export function setup() {
  const root = mkdtempSync(join(tmpdir(), "dori-install-"));
  roots.push(root);
  const upstream = join(root, "upstream");
  const src = join(root, "src");
  const home = join(root, "home");
  const bin = join(root, "bin");
  const ulw = join(root, "omp-ulw");
  const cli = "skills/dori-mode/scripts/src/cli.ts";
  mkdirSync(join(upstream, "skills/dori-mode/scripts/src"), { recursive: true });
  mkdirSync(join(upstream, "skills/dori-mode/references"), { recursive: true });
  mkdirSync(join(upstream, "extension"));
  writeFileSync(join(upstream, "extension/index.ts"), "export default () => {};\n");
  mkdirSync(ulw);
  writeFileSync(join(ulw, "index.ts"), "export default () => {};\n");
  writeFileSync(join(upstream, cli), "v1\n");
  writeFileSync(join(upstream, "omp-ulw.lock"), "0000000000000000000000000000000000000000\n");
  writeFileSync(join(upstream, "skills/dori-mode/SKILL.md"), skillFixture);
  writeFileSync(join(upstream, "skills/dori-mode/references/config.example.json"), "{}\n");
  git(upstream, "init", "-q", "-b", "main");
  git(upstream, "add", ".");
  git(upstream, "commit", "-qm", "v1");
  git(root, "clone", "-q", upstream, src);
  mkdirSync(home);
  mkdirSync(bin);
  // Record package-manager mutations without installing or linking anything on the host.
  writeFileSync(
    join(bin, "bun"),
    `#!/bin/sh\ncase "$1" in\n--version) echo "\${INSTALL_TEST_BUN_VERSION:-1.4.4}" ;;\n-e) exec ${JSON.stringify(process.execPath)} "$@" ;;\n*) echo "$*" >> ${JSON.stringify(join(root, "bun.log"))} ;;\nesac\n`,
  );
  chmodSync(join(bin, "bun"), 0o755);
  writeFileSync(
    join(bin, "omp"),
    "#!/bin/sh\ncase \"$1\" in\n--version) echo 'omp 18.18.8' ;;\n*) exit 64 ;;\nesac\n",
  );
  chmodSync(join(bin, "omp"), 0o755);
  const upstreamCommit = (text: string) => {
    writeFileSync(join(upstream, cli), text);
    git(upstream, "commit", "-qam", text.trim());
  };
  const env = (extra: Record<string, string> = {}) => ({
    ...gitEnv,
    HOME: home,
    BUN_INSTALL: join(home, ".bun"),
    DORI_INSTALL_MODE: "",
    DORI_NO_PULL: "",
    DORI_SRC: src,
    SKILLS_DIR: join(home, "skills"),
    OMP_EXTENSIONS_DIR: join(home, "extensions"),
    OMP_ULW_SRC: ulw,
    OMP_DORI_HOME: join(home, ".omp-dori"),
    OMP_DORI_HC_SESSION: "",
    OMP_DORI_BIN_DIR: join(home, "global-bin"),
    PATH: `${bin}:${process.env["PATH"]}`,
    ...extra,
  });
  const install = (extra: Record<string, string> = {}) => {
    const r = Bun.spawnSync([script], { env: env(extra), stdin: "ignore" });
    return { code: r.exitCode, out: r.stdout.toString(), err: r.stderr.toString() };
  };
  return { root, upstream, src, home, ulw, cli, upstreamCommit, env, install };
}
