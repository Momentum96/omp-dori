import { expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { defaultConfig, loadConfig } from "../skills/dori-mode/scripts/src/config.ts";
import { claimDone, closeLane, type FlowDeps } from "../skills/dori-mode/scripts/src/done-flow.ts";
import { freshnessTick } from "../skills/dori-mode/scripts/src/freshness.ts";
import { launchLane } from "../skills/dori-mode/scripts/src/launch.ts";
import { parkLane } from "../skills/dori-mode/scripts/src/park.ts";
import { pauseLane, resumeLane } from "../skills/dori-mode/scripts/src/pause.ts";
import { Registry, statusOf } from "../skills/dori-mode/scripts/src/registry.ts";
import { reopenLane } from "../skills/dori-mode/scripts/src/reopen.ts";
import { Reports } from "../skills/dori-mode/scripts/src/reports.ts";
import { type Runner, runtimesOf } from "../skills/dori-mode/scripts/src/run.ts";
import { laneStatus } from "../skills/dori-mode/scripts/src/status.ts";
import { tellLane } from "../skills/dori-mode/scripts/src/tell.ts";
import { shq } from "../src/delivery.ts";
import { HeadlessRuntime, headlessArgv } from "../src/runtime/headless.ts";
import { formatRef, parseRef, type SessionRef } from "../src/runtime/types.ts";

const fakeHost = resolve(import.meta.dir, "fixtures/headless-rpc.ts");
const skillCli = resolve(import.meta.dir, "../skills/dori-mode/scripts/src/cli.ts");
const noExternal: Runner = async () => ({ code: 1, out: "", err: "" });

function fixture() {
  const home = realpathSync(mkdtempSync("/tmp/cataly-headless-test-"));
  const stateDir = join(home, "state");
  const cwd = join(home, "work");
  mkdirSync(cwd);
  const omp = join(home, "omp");
  writeFileSync(omp, `#!/bin/sh\nexec ${shq(process.execPath)} ${shq(fakeHost)} "$@"\n`);
  chmodSync(omp, 0o700);
  const path = join(home, "config.json");
  const trace = join(home, "trace.jsonl");
  const saved = join(home, "saved.jsonl");
  const brief = join(home, "brief.txt");
  const evidence = join(cwd, "evidence.txt");
  writeFileSync(brief, "work");
  writeFileSync(evidence, "evidence");
  const env = {
    HOME: home,
    PATH: `/usr/bin:/bin:${dirname(process.execPath)}`,
    DORI_HOME: home,
    DORI_STATE_DIR: stateDir,
    DORI_CONFIG: path,
    PI_CODING_AGENT_DIR: join(home, "omp-data"),
    CATALY_TEST_TRACE: trace,
    CATALY_TEST_SESSION: saved,
    HERDR_PANE_ID: "w1:p2",
  };
  const config = {
    ...defaultConfig(home),
    stateDir,
    defaultCwd: cwd,
    laneRuntime: "headless" as const,
    runtime: "hc" as const,
    agentCommand: [omp, "--model", "{model}", "{prompt}"],
    nudgeAfterMin: 1,
  };
  writeFileSync(path, JSON.stringify(config));
  let now = Date.now();
  const deps: FlowDeps = {
    run: noExternal,
    registry: new Registry(stateDir),
    config,
    env,
    clock: { now: () => now, sleep: (ms) => Bun.sleep(ms) },
  };
  return {
    home,
    stateDir,
    env,
    path,
    saved,
    deps,
    input: { key: "headless-lane", title: "Native work", brief, done: `file ${evidence}` },
    advance: (ms: number) => {
      now += ms;
    },
    frames: () =>
      readFileSync(trace, "utf8")
        .trim()
        .split("\n")
        .map((line) => JSON.parse(line) as Record<string, unknown>),
  };
}

async function waitFor<T>(read: () => Promise<T | undefined>): Promise<T> {
  for (let tries = 0; tries < 100; tries++) {
    const value = await read();
    if (value !== undefined) return value;
    await Bun.sleep(30);
  }
  throw new Error("native readback did not reach the expected state");
}

test("headless runtime is opt-in, preserving the terminal default and explicit config precedence", async () => {
  const f = fixture();
  try {
    expect(defaultConfig(f.home).laneRuntime).toBeUndefined();
    expect((await loadConfig(f.path, f.home, f.env)).laneRuntime).toBe("headless");
    expect((await loadConfig(f.path, f.home, { ...f.env, DORI_LANE_RUNTIME: "terminal" })).laneRuntime).toBe(
      "terminal",
    );
    expect(runtimesOf(f.deps).default.kind).toBe("hc");
    expect(() => headlessArgv(["omp", "--mode", "json", "{prompt}"], "test")).toThrow();
    expect(() => headlessArgv(["omp", "--resume", "some-session", "{prompt}"], "test")).toThrow();
    expect(() => headlessArgv(["omp", "--dori-role", "dori", "{prompt}"], "test")).toThrow();
    expect(() => headlessArgv(["node", "-e", "work", "{prompt}"], "test")).toThrow();
  } finally {
    rmSync(f.home, { recursive: true, force: true });
  }
});

test.skipIf(process.platform === "win32")(
  "skill launch, report, claim, freshness, hold, and close use one native lane",
  async () => {
    const f = fixture();
    const rt = new HeadlessRuntime(f.stateDir, f.env);
    let ref: SessionRef | undefined;
    try {
      const launched = await launchLane(f.deps, f.input);
      const lane = launched.lane;
      ref = lane.sessionRef;
      expect(launched.startup).toStartWith("STARTUP_OK");
      if (!ref || !lane.session) throw new Error("native launch did not record its identity");
      const actual = ref;
      const session = lane.session;
      expect(parseRef(formatRef(ref))).toEqual(ref);
      expect(lane.sessionFile).toBe(f.saved);
      expect(lane.lifecycleEpoch).toBe(ref.id);
      expect(await rt.observe(ref)).toMatchObject({
        role: "lane",
        sessionId: session,
        goal: { status: "active" },
        held: false,
      });
      expect((await laneStatus(f.deps, lane.key)).lanes[0]?.runtime?.goal.status).toBe("active");

      const report = Bun.spawn(
        [process.execPath, skillCli, "report", lane.key, "--kind", "milestone", "--text", "native evidence", "--json"],
        { env: f.env, stdout: "pipe", stderr: "pipe" },
      );
      const [out, err, code] = await Promise.all([
        new Response(report.stdout).text(),
        new Response(report.stderr).text(),
        report.exited,
      ]);
      if (code !== 0) throw new Error(err || out);
      expect(JSON.parse(out)).toMatchObject({ generation: ref.id, text: "native evidence" });
      expect(new Reports(f.stateDir).read(lane.key, { generation: ref.id })?.lastReport?.text).toBe("native evidence");

      expect(await tellLane(f.deps, lane, "request approval")).toMatchObject({ code: 0 });
      const approval = await waitFor(async () =>
        (await rt.observe(actual)).dialogs.find((dialog) => dialog.kind === "approval"),
      );
      expect(await rt.deliver(ref, "unapproved input", { force: true, when: "any" })).toEqual({
        kind: "not-ready",
        status: "blocked",
      });
      expect(await closeLane(f.deps, lane, "no close with pending input")).toMatchObject({ closed: false });
      expect(await pauseLane(f.deps, lane, "wait")).toMatchObject({ code: 3 });
      f.advance(120_000);
      expect(await freshnessTick(f.deps, f.home)).toEqual(
        expect.arrayContaining([expect.objectContaining({ kind: "blocked" })]),
      );
      expect((await freshnessTick(f.deps, f.home)).some((entry) => entry.kind === "blocked")).toBeFalse();
      await expect(rt.answer(ref, approval.id, { value: "Deny" }, "different")).rejects.toThrow("does not match");
      await expect(rt.answer(ref, approval.id, { value: "Approve for this session" }, session)).rejects.toThrow(
        "unsupported",
      );
      expect(await rt.answer(ref, approval.id, { value: "Deny" }, session)).toEqual({ submitted: true });
      await expect(rt.answer(ref, approval.id, { value: "Deny" }, session)).rejects.toThrow("no matching");

      expect(await tellLane(f.deps, lane, "request question")).toMatchObject({ code: 0 });
      const question = await waitFor(async () =>
        (await rt.observe(actual)).dialogs.find((dialog) => dialog.kind === "ask"),
      );
      await expect(
        rt.answer(ref, question.id, { answers: [{ id: "single", selectedOptions: ["wrong"] }] }, session),
      ).rejects.toThrow("unsupported");
      expect(
        await rt.answer(ref, question.id, { answers: [{ id: "single", selectedOptions: ["B"] }] }, session),
      ).toEqual({ submitted: true });

      expect(await tellLane(f.deps, lane, "long turn")).toMatchObject({ code: 0 });
      await waitFor(async () => ((await rt.observe(actual)).status === "working" ? true : undefined));
      const latest = await f.deps.registry.read(lane.key);
      if (!latest) throw new Error("lost lane record");
      const paused = await pauseLane(f.deps, latest, "operator wait");
      expect(paused.code).toBe(0);
      expect((await rt.observe(ref)).goal?.status).toBe("paused");
      expect(await tellLane(f.deps, paused.lane, "do not wake")).toMatchObject({ code: 2 });
      expect(await freshnessTick(f.deps, f.home)).toEqual([]);
      const resumed = await resumeLane(f.deps, paused.lane, "operator go");
      expect(resumed.code).toBe(0);
      const message = "/only-literal `suffix` $(nothing)";
      expect(await tellLane(f.deps, resumed.lane, message)).toMatchObject({ code: 0 });
      const literal = `[CATALY message]\n${message}`;
      expect((await rt.observe(ref)).recent).toContainEqual({ kind: "assistant", text: literal });
      const claimed = await claimDone(f.deps, resumed.lane, f.saved);
      expect(claimed).toStartWith("LANE_DONE_CLAIMED");
      const claimedLane = await f.deps.registry.read(lane.key);
      if (!claimedLane) throw new Error("lost claim");
      expect(await closeLane(f.deps, claimedLane, "verified")).toMatchObject({ closed: true });
      expect(await rt.observe(ref)).toMatchObject({ status: "gone", held: true });
      expect(statusOf((await f.deps.registry.read(lane.key))!)).toBe("closed");
      const frames = f.frames();
      expect(frames.filter((frame) => frame["type"] === "extension_ui_response")).toHaveLength(2);
      expect(frames.filter((frame) => frame["type"] === "follow_up" && frame["message"] === literal)).toHaveLength(1);
      expect(frames.filter((frame) => frame["type"] === "goal" && frame["op"] === "create")).toHaveLength(1);
      expect(frames.filter((frame) => frame["type"] === "abort")).toHaveLength(1);
    } finally {
      if (ref) await rt.close(ref);
      rmSync(f.home, { recursive: true, force: true });
    }
  },
  25_000,
);

test.skipIf(process.platform === "win32")(
  "park and explicit reopen preserve native transcript identity without reusing a host",
  async () => {
    const f = fixture();
    const rt = new HeadlessRuntime(f.stateDir, f.env);
    const refs: SessionRef[] = [];
    try {
      const initial = (await launchLane(f.deps, f.input)).lane;
      if (!initial.sessionRef) throw new Error("native ref missing");
      refs.push(initial.sessionRef);
      const parked = await parkLane(f.deps, initial, "stop work");
      expect(parked.code).toBe(0);
      expect((await rt.observe(initial.sessionRef)).status).toBe("gone");
      const deps: FlowDeps = {
        ...f.deps,
        run: async (argv) => (argv[0] === "hc" ? { code: 0, out: '{"terminals":[]}', err: "" } : noExternal(argv)),
      };
      const opened = await reopenLane(deps, parked.lane);
      if (opened.lane.sessionRef) refs.push(opened.lane.sessionRef);
      expect(opened.lines[0]).toStartWith("REOPENED");
      expect(opened.code).toBe(0);
      expect(opened.lane.session).toBe(initial.session);
      expect(opened.lane.sessionRef?.id).not.toBe(initial.sessionRef.id);
      expect(opened.lane.sessionRef?.runtime).toBe("headless");
    } finally {
      for (const ref of refs) await rt.close(ref);
      rmSync(f.home, { recursive: true, force: true });
    }
  },
  15_000,
);
