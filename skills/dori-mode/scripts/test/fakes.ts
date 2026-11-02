import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { type DoriConfig, defaultConfig } from "../src/config.ts";
import type { FlowDeps } from "../src/done-flow.ts";
import { type PaneState, writePaneState } from "../src/lane-state.ts";
import { Registry } from "../src/registry.ts";
import type { Clock, Ran, Runner } from "../src/run.ts";

export type World = {
  prState: string;
  issueState: string;
  aheadCount: string;
  dirty: string;
  stuckReads: number;
  screen: string;
  panes: { pane_id: string; workspace_id: string; agent?: string; agent_status?: string; title?: string; cwd?: string }[];
  /** agent_status the target pane moves to when Enter is pressed (models a lane picking up the message). */
  onEnterStatus?: string;
  /** Screens returned by successive reads; falls back to `screen` when empty. */
  screens?: string[];
  /** Runs when text is typed into a pane (models the session reacting, e.g. the omp-dori extension writing its goal). */
  onSendText?: (pane: string, text: string) => void;
  /** What `ps -A` lists, and the shell pid `herdr pane process-info` reports per pane. */
  procs?: { pid: number; ppid: number; cmd: string }[];
  shells?: Record<string, number>;
  calls: string[][];
};

export const WT = "/repo/.wt/demo-lane";

export const newWorld = (): World => ({ prState: "MERGED", issueState: "CLOSED", aheadCount: "0", dirty: "", stuckReads: 0, screen: "❯ ", panes: [], calls: [] });

export const fakeRunner = (w: World): Runner => async (argv) => {
  w.calls.push([...argv]);
  const ok = (out = ""): Ran => ({ code: 0, out, err: "" });
  const [cmd, a1, a2] = argv;
  if (cmd === "herdr" && a1 === "pane" && a2 === "read") {
    if (w.stuckReads > 0) {
      w.stuckReads--;
      return ok("❯ [LEAD] stuck text still here");
    }
    return ok(w.screens?.length ? (w.screens.shift() ?? w.screen) : w.screen);
  }
  if (cmd === "herdr" && a1 === "pane" && a2 === "send-keys" && w.onEnterStatus) {
    const p = w.panes.find((x) => x.pane_id === argv[3]);
    if (p) p.agent_status = w.onEnterStatus;
    return ok();
  }
  if (cmd === "herdr" && a1 === "pane" && a2 === "send-text") {
    w.onSendText?.(argv[3] ?? "", argv[4] ?? "");
    return ok();
  }
  if (cmd === "herdr" && a1 === "pane" && a2 === "process-info") {
    const shell = w.shells?.[argv[4] ?? ""];
    return shell ? ok(JSON.stringify({ result: { process_info: { shell_pid: shell } } })) : { code: 1, out: "", err: "no such pane" };
  }
  if (cmd === "ps" && a1 === "-A") return ok((w.procs ?? []).map((p) => `${p.pid} ${p.ppid} ${p.cmd}`).join("\n"));
  if (cmd === "herdr" && a1 === "pane" && a2 === "list") return ok(JSON.stringify({ result: { panes: w.panes } }));
  if (cmd === "herdr") return ok();
  if (cmd === "git" && argv.includes("worktree") && argv.includes("list")) return ok(`worktree /repo\nbranch refs/heads/main\n\nworktree ${WT}\nbranch refs/heads/demo-lane`);
  if (cmd === "git" && argv.includes("status")) return ok(w.dirty);
  if (cmd === "git" && argv.includes("rev-list")) return ok(w.aheadCount);
  if (cmd === "git") return ok();
  if (cmd === "gh" && a1 === "pr") return ok(JSON.stringify({ state: w.prState, mergeCommit: w.prState === "MERGED" ? { oid: "abc1234567890" } : null }));
  if (cmd === "gh" && a1 === "issue") return ok(JSON.stringify({ state: w.issueState }));
  if (cmd === "notify") return ok();
  return { code: 1, out: "", err: `unexpected ${argv.join(" ")}` };
};

export const fakeClock = (start: number): Clock & { at: number } => {
  const c = { at: start, now: () => c.at, sleep: async () => {} };
  return c;
};

export const withState = (): { dir: string; done: () => void } => {
  const dir = mkdtempSync(join(tmpdir(), "dori-test-"));
  return { dir, done: () => rmSync(dir, { recursive: true, force: true }) };
};

export const depsFor = (w: World, clock: Clock, stateDir: string, patch: Partial<DoriConfig> = {}): FlowDeps => {
  const config: DoriConfig = { ...defaultConfig("/home/test"), stateDir, defaultCwd: "/repo", leadPane: "lead:p1", ...patch };
  return { run: fakeRunner(w), clock, registry: new Registry(stateDir), config, exists: () => true };
};

/** Runs an omp process `pid` (laneState's default) under the shell of `pane`, as herdr and ps would report it. */
export const agentIn = (w: World, pane: string, pid = 4242): void => {
  const shell = pid - 1;
  w.shells = { ...w.shells, [pane]: shell };
  w.procs = [...(w.procs ?? []), { pid: shell, ppid: 1, cmd: "zsh" }, { pid, ppid: shell, cmd: "bun /Users/me/.bun/bin/omp --dori-role lane" }];
};

/** Writes the pane state the omp-dori extension keeps for a lane session; `patch` overrides any field. */
export const laneState = (stateDir: string, paneId: string, patch: Partial<PaneState> = {}): void =>
  writePaneState(stateDir, {
    paneId,
    role: "lane",
    sessionId: "01a11440-ee81-7f90-b2ed-dcc7d51a0adf",
    cwd: "/repo",
    pid: 4242,
    startedAt: "2026-01-01T12:00:00.000Z",
    updatedAt: "2026-01-01T12:00:00.000Z",
    goal: { status: "active", objective: "ship it" },
    approvalPending: false,
    ...patch,
  });

export const sent = (w: World): string[] => w.calls.filter((c) => c[0] === "herdr" && c[2] === "send-text").map((c) => c[4] ?? "");
