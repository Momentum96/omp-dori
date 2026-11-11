import { afterEach } from "bun:test";
import {
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readlinkSync,
  realpathSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { DoriUpdater } from "../src/update/controller.ts";
import type { UpdateConfig, UpdateEvent } from "../src/update/types.ts";

const directories: string[] = [];
afterEach(() => {
  for (const path of directories.splice(0)) {
    for (const receipt of new Bun.Glob("**/runtime-paths.json").scanSync({
      cwd: path,
      absolute: true,
      dot: true,
      onlyFiles: true,
      followSymlinks: false,
    })) {
      const ownership = JSON.parse(readFileSync(receipt, "utf8")) as { alias: string; directory: string };
      if (
        typeof ownership.alias !== "string" ||
        !/^\/tmp\/du-[a-f0-9]{16}$/.test(ownership.alias) ||
        ownership.directory !== realpathSync(dirname(receipt))
      )
        continue;
      const alias = lstatSync(ownership.alias, { throwIfNoEntry: false });
      if (alias?.isSymbolicLink() && readlinkSync(ownership.alias) === ownership.directory) unlinkSync(ownership.alias);
    }
    rmSync(path, { recursive: true, force: true });
  }
});

export function git(cwd: string, ...argv: string[]): string {
  const result = Bun.spawnSync(["git", "-c", "core.hooksPath=/dev/null", ...argv], {
    cwd,
    env: { PATH: process.env["PATH"], HOME: cwd, GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null" },
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode !== 0) throw new Error(result.stderr.toString());
  return result.stdout.toString().trim();
}

export function fixture() {
  const directory = mkdtempSync(join(tmpdir(), "dori-update-test-"));
  directories.push(directory);
  const remote = join(directory, "remote");
  const localSource = join(directory, "local");
  mkdirSync(remote);
  mkdirSync(localSource);
  const write = (root: string, path: string, text: string): void => {
    mkdirSync(dirname(join(root, path)), { recursive: true });
    writeFileSync(join(root, path), text);
  };
  for (const root of [remote, localSource]) {
    write(root, "package.json", '{"name":"dori-superagent","type":"module"}\n');
    write(root, "skills/dori-mode/scripts/src/cli.ts", "export const cli = 1;\n");
    write(root, "src/behavior.ts", "export const behavior = 1;\n");
    write(root, "omp-ulw.lock", `${"a".repeat(40)}\n`);
  }
  git(remote, "init", "-b", "main");
  git(remote, "config", "user.name", "Dori updater fixture");
  git(remote, "config", "user.email", "updater@example.invalid");
  git(remote, "add", ".");
  git(remote, "commit", "-m", "Dori fixture baseline");
  const baseUpstream = git(remote, "rev-parse", "HEAD");
  const config: UpdateConfig = {
    version: 1,
    target: "dori",
    runtime: "herdr",
    root: join(directory, "updates"),
    remote,
    ref: "refs/heads/main",
    baseUpstream,
    localSource,
    intervalSeconds: 1,
    // A real failing process exercises failure propagation; it never impersonates a successful coding agent.
    worker: { name: "port", argv: ["false", "{prompt}"], timeoutMs: 10_000 },
    reviewer: {
      name: "review",
      argv: ["omp", "-p", "--model", "anthropic/claude-opus-5-5", "{prompt}"],
      timeoutMs: 10_000,
    },
    provision: [],
    checks: [{ name: "types", argv: ["bun", "run", "typecheck"], timeoutMs: 10_000 }],
    modelEnv: [],
    reporterEnv: [],
  };
  const events: UpdateEvent[] = [];
  const updater = new DoriUpdater(config, (event) => events.push(event));
  return { directory, remote, localSource, config, updater, events, write };
}
