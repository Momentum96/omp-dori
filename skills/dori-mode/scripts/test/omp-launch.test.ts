import { afterEach, beforeEach, expect, test } from "bun:test";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import type { PaneState } from "../src/lane-state.ts";
import { launchLane } from "../src/launch.ts";
import { sessionDirFor } from "../src/session-id.ts";
import { agentIn, depsFor, fakeClock, laneState, newWorld, withState, type TestState, type World } from "./fakes.ts";

const T0 = Date.parse("2026-01-01T12:00:00.000Z");
let state: TestState;
let world: World;
let n = 0;
beforeEach(() => {
  state = withState();
  world = newWorld();
});
afterEach(() => state.done());

const start = async (
  key: string,
  patch: Partial<PaneState> = {},
  options: {
    firstLane?: string;
    at?: string;
    screen?: string;
    slow?: boolean;
    packageCommand?: boolean;
    model?: string;
    startDelay?: number;
    suspended?: boolean;
  } = {},
) => {
  const brief = join(state.dir, `${key}.md`);
  writeFileSync(brief, "# Ship the fixture\n");
  const sessionsDir = join(state.dir, "omp-store");
  const dir = sessionDirFor(sessionsDir, "/repo");
  mkdirSync(dir, { recursive: true });
  const id = `01a1b6d8-0000-7000-8000-${String(++n).padStart(12, "0")}`;
  const at = options.at ?? "2026-01-01T12:00:03.000Z";
  const sessionFile = join(dir, `${at.replace(/:/g, "-").replace(".", "-")}_${id}.jsonl`);
  writeFileSync(
    sessionFile,
    [
      { type: "session", id, timestamp: at, cwd: "/repo" },
      {
        type: "message",
        message: {
          role: "user",
          content: [
            {
              type: "text",
              text: `Read the brief at ${brief}. You are the ${options.firstLane ?? key} lane; report as the footer says.`,
            },
          ],
        },
      },
    ]
      .map((entry) => JSON.stringify(entry))
      .join("\n") + "\n",
  );
  world = newWorld();
  world.screen = options.screen ?? "Working (12s)\nprompt\nGoal: ...";
  world.panes = [{ pane_id: "w:p9", workspace_id: "w", agent: "omp", agent_status: "working" }];
  if (options.suspended && world.panes[0]) world.panes[0].foreground = "zsh";
  agentIn(world, "w:p9");
  laneState(state.dir, "w:p9", { sessionId: id, sessionFile, ...patch });
  const clock = fakeClock(T0);
  const base = depsFor(
    world,
    {
      ...clock,
      sleep: async (ms) => {
        clock.at += ms;
        if (options.slow && ms === 10_000)
          laneState(state.dir, "w:p9", { sessionId: id, sessionFile, goal: { status: "active" } });
      },
    },
    state.dir,
    {
      sessionsDir,
      ...(options.packageCommand
        ? {
            agentCommand: [
              "bun",
              "/opt/node_modules/@oh-my-pi/pi-coding-agent/dist/cli.js",
              "--dori-role",
              "lane",
              "--model",
              "{model}",
              "{prompt}",
            ],
          }
        : {}),
    },
  );
  const deps = {
    ...base,
    run: async (argv: readonly string[]) => {
      if (argv[0] === "herdr" && argv[1] === "tab" && argv[2] === "create")
        return {
          code: 0,
          out: JSON.stringify({ result: { root_pane: { pane_id: "w:p9" }, tab_id: "w:t9" } }),
          err: "",
        };
      if (argv[0] === "herdr" && argv[1] === "pane" && argv[2] === "run") clock.at += options.startDelay ?? 0;
      return base.run(argv);
    },
  };
  return launchLane(deps, {
    key,
    title: "fixture",
    brief,
    done: "closed acme/app#3",
    ...(options.model === undefined ? {} : { model: options.model }),
  });
};

test("OMP startup reads its own live goal and session launch prompt even when the footer is truncated", async () => {
  expect((await start("active-lane")).startup).toBe("STARTUP_OK active-lane w:p9");
  const command = world.calls.find((argv) => argv[2] === "run")?.at(-1) ?? "";
  expect(command).toContain("omp --dori-role lane --model anthropic/claude-opus-5-5");
  expect(command).toContain("ulw set goal and work");
  expect((await start("package-lane", {}, { packageCommand: true })).startup).toBe("STARTUP_OK package-lane w:p9");
  const overridden = await start("override-lane", {}, { model: "provider/explicit-model" });
  expect(overridden.lane.model).toBe("provider/explicit-model");
  expect(world.calls.find((argv) => argv[2] === "run")?.at(-1)).toContain("--model provider/explicit-model");
  expect((await start("slow-lane", { goal: { status: "none" } }, { slow: true })).startup).toBe(
    "STARTUP_OK slow-lane w:p9",
  );
  expect((await start("slow-cli", {}, { startDelay: 15_000 })).startup).toBe("STARTUP_OK slow-cli w:p9");
});

test("a quoted omo footer, another lane's prompt or an old session cannot satisfy OMP's startup goal", async () => {
  for (const status of ["none", "paused", "blocked", "complete", "dropped"] as const) {
    const key = `${status}-lane`;
    expect((await start(key, { goal: { status } }, { screen: "prompt\nPursuing goal (1m)" })).startup).toStartWith(
      `STARTUP_NO_GOAL ${key} w:p9`,
    );
  }
  expect((await start("foreign-lane", {}, { firstLane: "other-lane" })).startup).toStartWith(
    "STARTUP_NO_GOAL foreign-lane w:p9",
  );
  expect((await start("stale-lane", {}, { at: "2026-01-01T11:50:00.000Z" })).startup).toStartWith(
    "STARTUP_NO_GOAL stale-lane w:p9",
  );
  expect((await start("lead-role", { role: "dori" })).startup).toStartWith("STARTUP_NO_GOAL lead-role w:p9");
  expect((await start("dead-writer", { pid: 77 })).startup).toStartWith("STARTUP_UNCONFIRMED dead-writer w:p9");
  expect((await start("approval-lane", { approvalPending: true })).startup).toStartWith(
    "STARTUP_UNCONFIRMED approval-lane w:p9",
  );
  expect((await start("suspended-lane", {}, { suspended: true })).startup).toStartWith(
    "STARTUP_UNCONFIRMED suspended-lane w:p9",
  );
});

test("OMP extension, flag and model failures retain STARTUP_ERROR classifications", async () => {
  const failures = [
    ["flag-lane", "Error: unknown flag: --dori-role", "unknown flag: "],
    ["extension-lane", "Failed to load extension /local/omp-dori: boom", "Failed to load extension"],
    ["trusted-lane", "Trusted extension failed to load: boom", "Trusted extension failed to load"],
    ["model-lane", "Model provider/missing not found", "Model provider/missing not found"],
    ["empty-models", "No models available.", "No models available"],
  ];
  for (const [key = "", screen = "", phrase = ""] of failures)
    expect((await start(key, {}, { screen })).startup).toBe(`STARTUP_ERROR ${key} w:p9: ${phrase}`);
});
