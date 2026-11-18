import { afterEach, expect, test } from "bun:test";
import { lstatSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { paneStateDir, writePaneState } from "../src/lane-state.ts";

const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

const state = {
  paneId: "t9",
  role: "dori" as const,
  sessionId: "fixture",
  sessionFile: "/tmp/fixture.jsonl",
  cwd: "/tmp",
  pid: process.pid,
  startedAt: new Date(0).toISOString(),
  agentStatus: "idle" as const,
  approvalPending: false,
};

test("pane state stays private to the owner under a group-writable umask", () => {
  const root = mkdtempSync(join(tmpdir(), "lane-state-"));
  roots.push(root);
  // Admission on every host requires no group/other bits; a 002 umask must not leak them.
  const previous = process.umask(0o002);
  try {
    writePaneState(root, state);
  } finally {
    process.umask(previous);
  }
  const file = join(paneStateDir(root), "t9.json");
  expect(lstatSync(file).mode & 0o777).toBe(0o600);
  expect(lstatSync(paneStateDir(root)).mode & 0o777).toBe(0o700);
  writePaneState(root, { ...state, sessionId: "rewritten" });
  expect(lstatSync(file).mode & 0o777).toBe(0o600);
});
