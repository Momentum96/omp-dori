import { afterEach, beforeEach, expect, test } from "bun:test";
import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { DoriConfig } from "../src/config.ts";
import { adoptLane, LaunchError, prepareLaunch } from "../src/launch.ts";
import { cliCommand, shellQuote } from "../src/launch-footer.ts";
import type { Lane } from "../src/registry.ts";
import { Reports } from "../src/reports.ts";
import { depsFor, fakeClock, newWorld, withState, type World } from "./fakes.ts";

const T0 = Date.parse("2026-01-01T12:00:00.000Z");

let state: ReturnType<typeof withState>;

let world: World;

beforeEach(() => {
  state = withState();
  world = newWorld();
});

afterEach(() => state.done());

const working: Lane = {
  key: "quiet-lane",
  title: "quiet",
  thread: "chat:team/9",
  pane: "w:p7",
  brief: "/b.md",
  done: "merged acme/app#1",
  openedAt: new Date(T0).toISOString(),
  lastReplyAt: T0,
};

test("adopting a lane refuses a Done line the watcher could never check", async () => {
  const deps = depsFor(world, fakeClock(T0), state.dir);
  await expect(
    adoptLane(deps, { key: "x-lane", title: "x", brief: "/b.md", done: "two clean QA passes", pane: "w:p2" }),
  ).rejects.toBeInstanceOf(LaunchError);
  const ok = await adoptLane(deps, {
    key: "x-lane",
    title: "x",
    brief: "/b.md",
    done: "closed acme/app#3",
    pane: "w:p2",
  });
  expect(ok.thread).toBe("none");
  await expect(
    adoptLane(deps, { key: "x-lane", title: "x", brief: "/b.md", done: "closed acme/app#3", pane: "w:p2" }),
  ).rejects.toThrow("already registered");
});

test("the footer CLI command survives a skill path with a space or a quote", async () => {
  const dir = mkdtempSync(join(tmpdir(), "dori skill's dir "));
  try {
    const cli = join(dir, "cli.ts");
    writeFileSync(cli, 'console.log("CLI_OK " + process.argv.slice(2).join(" "));\n');
    const command = cliCommand(cli);
    const proc = Bun.spawn(["bash", "-c", `${command} report path-lane --kind milestone`], {
      stdout: "pipe",
      stderr: "pipe",
    });
    expect((await new Response(proc.stdout).text()).trim()).toBe("CLI_OK report path-lane --kind milestone");
    expect(await proc.exited).toBe(0);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("with leadInbox the footer names no lead pane, and its report reaches the node's journal over an env.sh environment", async () => {
  // Given a node home whose config sets leadInbox, and a lane shell whose env.sh points at the install's own home
  const dir = mkdtempSync(join(tmpdir(), "dori inbox footer "));
  const priorConfig = process.env["DORI_CONFIG"];
  try {
    const configFile = join(dir, "config.json");
    process.env["DORI_CONFIG"] = configFile;
    const brief = join(dir, "brief.md");
    writeFileSync(brief, "Perform the fixture work.");
    const deps = depsFor(world, fakeClock(T0), state.dir);
    const config = { ...deps.config, runtime: "hc" as const, leadPane: "t2rg", leadInbox: true };
    writeFileSync(configFile, JSON.stringify(config));
    const prepared = await prepareLaunch(
      { ...deps, config },
      { key: "inbox-lane", title: "inbox", brief, done: working.done },
    );
    // Then the footer never names the lead pane or a send command and says the report alone delivers
    expect(prepared.preamble).not.toContain("t2rg");
    expect(prepared.preamble).not.toContain("hc send");
    expect(prepared.preamble).toContain("출력된 줄을 어디에도 보내지 말 것");
    expect(prepared.preamble).toContain("질문 도구(question tool)는 쓰지 말 것");
    await deps.registry.write(prepared.lane);
    const command = prepared.preamble.split("\n").find((line) => line.startsWith("line=$("));
    if (!command) throw new Error("launch footer has no report command");
    // When the lane runs it with the install's DORI_CONFIG and DORI_STATE_DIR exported
    const other = join(dir, "install-state");
    const proc = Bun.spawn(["bash", "-c", command], {
      stdout: "pipe",
      stderr: "pipe",
      env: {
        ...process.env,
        DORI_CONFIG: join(dir, "install-config.json"),
        DORI_STATE_DIR: other,
        kind: "blocker",
        text: "waiting on the owner",
      },
    });
    expect((await new Response(proc.stdout).text()).trimEnd()).toBe(
      "[REPORT] inbox-lane | blocker | waiting on the owner",
    );
    expect(await proc.exited).toBe(0);
    // Then the report lands in the node's journal, not the install's
    expect(new Reports(state.dir).read("inbox-lane")?.lastReport?.kind).toBe("blocker");
    expect(existsSync(join(other, "lanes"))).toBe(false);
  } finally {
    if (priorConfig === undefined) delete process.env["DORI_CONFIG"];
    else process.env["DORI_CONFIG"] = priorConfig;
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the selected footer runtime receives one data argument and missing destinations still journal reports", async () => {
  const dir = mkdtempSync(join(tmpdir(), "dori footer send "));
  const priorConfig = process.env["DORI_CONFIG"];
  try {
    const configFile = join(dir, "config.json");
    process.env["DORI_CONFIG"] = configFile;
    const brief = join(dir, "brief.md");
    writeFileSync(brief, "Perform the fixture work.");
    for (const bin of ["herdr", "hc"])
      writeFileSync(
        join(dir, bin),
        `#!/usr/bin/env bun\nawait Bun.write(process.env.ARGV_LOG, JSON.stringify([${JSON.stringify(bin)}, ...process.argv.slice(2)]));\n`,
        { mode: 0o755 },
      );
    const marker = join(dir, "must not execute");
    const text = `it's "done"; $(touch ${shellQuote(marker)}) \`touch ${shellQuote(marker)}\` \\ $HOME | 100%
next line`;
    const cases: Array<{
      configured?: DoriConfig["runtime"];
      env: Record<string, string>;
      selected?: DoriConfig["runtime"];
      lead?: string;
    }> = [
      { configured: "herdr", env: {}, selected: "herdr" },
      { configured: "hc", env: {}, selected: "hc" },
      { configured: "herdr", env: { DORI_RUNTIME: "hc" }, selected: "hc" },
      { env: { HC_TERMINAL_ID: "t1" }, selected: "hc" },
      { env: { HERDR_PANE_ID: "w:p1" }, selected: "herdr" },
      { env: { PATH: dir }, selected: "hc" },
      { configured: "herdr", env: {}, lead: "" },
      { env: {} },
    ];
    for (const [index, fixture] of cases.entries()) {
      const deps = depsFor(world, fakeClock(T0), state.dir);
      const config = { ...deps.config, leadPane: fixture.lead ?? "lead's pane" };
      delete config.runtime;
      if (fixture.configured) config.runtime = fixture.configured;
      writeFileSync(configFile, JSON.stringify(config));
      const key = `send-lane-${index}`;
      const prepared = await prepareLaunch(
        { ...deps, config, env: { PATH: "", ...fixture.env } },
        { key, title: "argv", brief, done: working.done },
      );
      expect(world.calls).toEqual([]);
      expect(await Bun.file(brief).text()).toBe("Perform the fixture work.");
      expect(await deps.registry.read(key)).toBeNull();
      await deps.registry.write(prepared.lane);
      const command = prepared.preamble.split("\n").find((line) => line.startsWith("line=$("));
      if (!command) throw new Error("launch footer has no report command");
      const argvLog = join(dir, `${index}.argv.json`);
      const proc = Bun.spawn(["bash", "-c", command], {
        stdout: "pipe",
        stderr: "pipe",
        env: {
          ...process.env,
          PATH: `${dir}:${process.env["PATH"]}`,
          DORI_CONFIG: configFile,
          DORI_STATE_DIR: state.dir,
          ARGV_LOG: argvLog,
          kind: "milestone",
          text,
        },
      });
      const report = `[REPORT] ${key} | milestone | ${text.replace("\n", " ")}`;
      expect((await new Response(proc.stdout).text()).trimEnd()).toBe(report);
      expect(await new Response(proc.stderr).text()).toBe("");
      expect(await proc.exited).toBe(0);
      expect(new Reports(state.dir).read(key)?.lastReport?.text).toBe(text);
      if (fixture.selected) {
        const sent =
          fixture.selected === "herdr"
            ? ["pane", "run", config.leadPane, report]
            : ["send", config.leadPane, "--", report];
        expect(await Bun.file(argvLog).json()).toEqual([fixture.selected, ...sent]);
      } else expect(existsSync(argvLog)).toBe(false);
      expect(existsSync(marker)).toBe(false);
    }
  } finally {
    if (priorConfig === undefined) delete process.env["DORI_CONFIG"];
    else process.env["DORI_CONFIG"] = priorConfig;
    rmSync(dir, { recursive: true, force: true });
  }
}, 15_000);
