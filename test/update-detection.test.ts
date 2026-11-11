import { describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { UpdateInterrupted } from "../src/update/config.ts";
import { DoriUpdater } from "../src/update/controller.ts";
import { detectHead } from "../src/update/prepare.ts";
import type { CommandReceipt, UpdateState } from "../src/update/types.ts";
import { fixture } from "./update-setup.ts";

type DetectionReceipt = CommandReceipt & { readonly receiptPath: string };
const detectionReceipt = (root: string): DetectionReceipt =>
  JSON.parse(readFileSync(join(root, "detector", "last-detection.json"), "utf8")) as DetectionReceipt;

// Real inherited descriptors require subprocess handshakes; fake time cannot drive pipe EOF or child exit.
async function waitFor(check: () => boolean): Promise<void> {
  const deadline = Date.now() + 8_000;
  while (!check()) {
    if (Date.now() > deadline) throw new Error("detector fixture did not reach its handshake");
    await Bun.sleep(20);
  }
}

function inheritedStderr(mode: "exit" | "abort") {
  const f = fixture();
  const bin = join(f.directory, "bin");
  mkdirSync(bin, { mode: 0o700 });
  const identity = join(f.directory, "descendant.json");
  const ready = join(f.directory, "git-ready.json");
  const stop = join(f.directory, "stop-descendant");
  const stopped = join(f.directory, "descendant-stopped.json");
  const result = join(f.directory, "detector-result.json");
  const token = randomUUID();
  f.write(
    f.directory,
    "descendant.ts",
    `
import { existsSync, writeFileSync, writeSync } from "node:fs";
const [identity, stop, stopped, token] = process.argv.slice(2);
const evidence = JSON.stringify({ pid: process.pid, token });
writeSync(2, "detector stderr before inherited hold\\n");
writeFileSync(identity, evidence);
const finish = () => {
  try { writeSync(2, "late descendant stderr\\n"); } catch {}
  writeFileSync(stopped, evidence);
  process.exit(0);
};
setTimeout(finish, 15_000);
while (!existsSync(stop)) await Bun.sleep(20);
finish();
`,
  );
  writeFileSync(
    join(bin, "git"),
    `#!${process.execPath}
import { existsSync, writeFileSync, writeSync } from "node:fs";
if (!process.argv.includes("ls-remote")) throw new Error("only the detector may invoke fixture git");
const descendant = Bun.spawn(${JSON.stringify([
      process.execPath,
      join(f.directory, "descendant.ts"),
      identity,
      stop,
      stopped,
      token,
    ])}, { stdin: "ignore", stdout: "ignore", stderr: "inherit" });
descendant.unref();
while (!existsSync(${JSON.stringify(identity)})) await Bun.sleep(20);
writeSync(1, ${JSON.stringify(`${f.config.baseUpstream}\t${f.config.ref}\n`)});
writeFileSync(${JSON.stringify(ready)}, JSON.stringify({ pid: process.pid }));
if (${JSON.stringify(mode)} === "exit") process.exit(0);
await Bun.sleep(20_000);
`,
    { mode: 0o700 },
  );
  f.write(
    f.directory,
    "detector.ts",
    `
import { existsSync, writeFileSync } from "node:fs";
import { DoriUpdater } from ${JSON.stringify(new URL("../src/update/controller.ts", import.meta.url).href)};
import { UpdateInterrupted } from ${JSON.stringify(new URL("../src/update/config.ts", import.meta.url).href)};
const controller = new AbortController();
const updater = new DoriUpdater(${JSON.stringify(f.config)}, () => {}, controller.signal);
const pending = updater.once("stage").then(
  (value) => ({ interrupted: false, outcome: value.outcome }),
  (error) => ({ interrupted: error instanceof UpdateInterrupted }),
);
if (${JSON.stringify(mode)} === "abort") {
  const deadline = Date.now() + 8_000;
  while (!existsSync(${JSON.stringify(ready)})) {
    if (Date.now() > deadline) throw new Error("fixture git never became ready");
    await Bun.sleep(20);
  }
  controller.abort();
}
writeFileSync(${JSON.stringify(result)}, JSON.stringify({ ...await pending, state: updater.status().state }));
`,
  );
  const proc = Bun.spawn([process.execPath, join(f.directory, "detector.ts")], {
    cwd: f.directory,
    env: { PATH: `${bin}:${process.env["PATH"] ?? "/usr/bin:/bin"}` },
    stdin: "ignore",
    stdout: "ignore",
    stderr: "pipe",
    timeout: 20_000,
  });
  const stderr = new Response(proc.stderr).text();
  return {
    ...f,
    proc,
    stderr,
    ready,
    result,
    stopped,
    cleanup: async () => {
      writeFileSync(stop, "release only this fixture's descendant");
      await proc.exited;
      await stderr;
      if (existsSync(identity)) {
        await waitFor(() => existsSync(stopped));
        const started = JSON.parse(readFileSync(identity, "utf8")) as { pid: number; token: string };
        expect(started.pid).toBeGreaterThan(0);
        expect(started.token).toBe(token);
        expect(JSON.parse(readFileSync(stopped, "utf8"))).toEqual(started);
      }
    },
  };
}

describe("bounded configured ref detection", () => {
  test("resolves the configured head and retains separate private invocation receipts", async () => {
    const f = fixture();
    expect(await detectHead(f.updater.context)).toBe(f.config.baseUpstream);
    const first = detectionReceipt(f.config.root);
    const stdout = readFileSync(first.stdout, "utf8");
    expect(await detectHead(f.updater.context)).toBe(f.config.baseUpstream);
    const second = detectionReceipt(f.config.root);
    expect(second.stdout).not.toBe(first.stdout);
    expect(second.stderr).not.toBe(first.stderr);
    expect(second.receiptPath).not.toBe(first.receiptPath);
    expect(readFileSync(first.stdout, "utf8")).toBe(stdout);
    for (const { receiptPath, ...receipt } of [first, second]) {
      expect(receipt.pid).toBeGreaterThan(0);
      expect(receipt.exitCode).toBe(0);
      expect(receipt.error).toBeUndefined();
      expect(receipt.finishedAt).toBeDefined();
      expect(JSON.parse(readFileSync(receiptPath, "utf8"))).toEqual(receipt);
      for (const path of [receiptPath, receipt.stdout, receipt.stderr]) {
        expect(statSync(path).mode & 0o077).toBe(0);
      }
    }
  });

  test("an already-aborted stage launches no detector and records no source attempt", async () => {
    const f = fixture();
    const controller = new AbortController();
    controller.abort();
    const updater = new DoriUpdater(f.config, () => {}, controller.signal);
    const error = await updater.once("stage").catch((error: unknown) => error);
    expect(error).toBeInstanceOf(UpdateInterrupted);
    expect(updater.status().state.jobs).toEqual({});
    expect(updater.status().state.observed).toBeUndefined();
    expect(existsSync(join(f.config.root, "detector"))).toBe(false);
    expect(existsSync(join(f.config.root, "jobs"))).toBe(false);
  });

  test.each(["exit", "abort"] as const)(
    "%s with a surviving inherited stderr is interrupted before parsing or staging",
    async (mode) => {
      const f = inheritedStderr(mode);
      let paths: string[] = [];
      let before: string[] = [];
      try {
        await waitFor(() => existsSync(f.result));
        expect(await f.proc.exited).toBe(0);
        expect(await f.stderr).toBe("");
        const result = JSON.parse(readFileSync(f.result, "utf8")) as {
          interrupted: boolean;
          state: UpdateState;
        };
        expect(result.interrupted).toBe(true);
        expect(result.state.jobs).toEqual({});
        expect(result.state.observed).toBeUndefined();
        expect(result.state.active).toBeUndefined();
        expect(existsSync(join(f.config.root, "jobs"))).toBe(false);
        const receipt = detectionReceipt(f.config.root);
        const direct = JSON.parse(readFileSync(f.ready, "utf8")) as { pid: number };
        expect(receipt.pid).toBe(direct.pid);
        expect(receipt.finishedAt).toBeDefined();
        expect(receipt.error).toContain("output is incomplete");
        expect(receipt.error).toContain("descendants may still be running");
        if (mode === "exit") expect(receipt.exitCode).toBe(0);
        else expect(receipt.signal).toBe("SIGKILL");
        const { receiptPath, ...local } = receipt;
        expect(JSON.parse(readFileSync(receiptPath, "utf8"))).toEqual(local);
        expect(readFileSync(receipt.stdout, "utf8")).toContain(f.config.baseUpstream);
        expect(readFileSync(receipt.stderr, "utf8")).toContain("detector stderr before inherited hold");
        expect(existsSync(f.stopped)).toBe(false);
        paths = [receipt.stdout, receipt.stderr];
        before = paths.map((path) => readFileSync(path, "utf8"));
      } finally {
        await f.cleanup();
      }
      expect(paths.map((path) => readFileSync(path, "utf8"))).toEqual(before);
    },
    30_000,
  );
});
