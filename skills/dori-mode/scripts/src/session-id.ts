import { readdirSync, realpathSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { isAbsolute, join, relative, resolve } from "node:path";

import { type PaneState, readPaneState } from "./lane-state.ts";
import type { Runner } from "./run.ts";

export type Proc = { readonly pid: number; readonly ppid: number; readonly cmd: string };
export type SessionHit = { readonly id: string; readonly via: "extension" | "start-time" | "none"; readonly pid?: number };

const AGENT = /senpi\/dist\/bundle\/cli\.js|(^|\/)omo(\s|$)|(^|\/)omp(\s|$)|pi-coding-agent\/dist\/cli\.js/;
const SESSION_FILE = /^(\d{4}-\d\d-\d\dT\d\d)-(\d\d)-(\d\d)-(\d{3})Z_([0-9a-f-]{36})\.jsonl$/;
const START_WINDOW_MS = 180_000;

export const procTable = async (run: Runner): Promise<Proc[]> =>
  (await run(["ps", "-A", "-o", "pid=,ppid=,command="])).out.split("\n").flatMap((line) => {
    const m = /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line);
    return m ? [{ pid: Number(m[1]), ppid: Number(m[2]), cmd: m[3] ?? "" }] : [];
  });

const descendants = (table: readonly Proc[], root: number): Proc[] => {
  const byParent = new Map<number, Proc[]>();
  for (const p of table) byParent.set(p.ppid, [...(byParent.get(p.ppid) ?? []), p]);
  const out: Proc[] = [];
  const queue = [...(byParent.get(root) ?? [])];
  for (let p = queue.shift(); p; p = queue.shift()) {
    out.push(p);
    queue.push(...(byParent.get(p.pid) ?? []));
  }
  return out;
};

// omp's resolveEquivalentPath: the real path when it exists, else the resolved one.
const canonical = (path: string): string => {
  try {
    return realpathSync(resolve(path));
  } catch {
    return resolve(path);
  }
};

const isWithin = (rel: string): boolean => rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));

const encodeRelative = (prefix: string, rel: string): string => {
  const encoded = rel.replace(/[/\\:]/g, "-");
  return encoded ? (prefix.endsWith("-") ? `${prefix}${encoded}` : `${prefix}-${encoded}`) : prefix;
};

/**
 * omp's session folder for a cwd (omp src/session/session-paths.ts getDefaultSessionDirName): under the temp root
 * `-tmp-<rel>`, else under home `-<rel>`, else the legacy absolute `--<path>--`. The temp root is checked first.
 */
export const sessionDirFor = (sessionsDir: string, cwd: string, home = homedir(), tmp = tmpdir()): string => {
  const at = canonical(cwd);
  const inTmp = relative(canonical(tmp), at);
  const inHome = relative(canonical(home), at);
  const name = isWithin(inTmp)
    ? encodeRelative("-tmp", inTmp)
    : isWithin(inHome)
      ? encodeRelative("-", inHome)
      : `--${at.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--`;
  return join(sessionsDir, name);
};

type PaneProcs = { readonly under: readonly Proc[]; readonly state: PaneState | null };

/**
 * The processes under a pane's shell, and the omp-dori state for the pane only if one of them wrote it. A file whose
 * writer has exited, or whose pid now belongs to a process outside this pane, is stale and reads as absent.
 */
const paneProcs = async (run: Runner, stateDir: string, pane: string, table: readonly Proc[]): Promise<PaneProcs> => {
  const info = await run(["herdr", "pane", "process-info", "--pane", pane]);
  const shellPid = info.code === 0 ? Number((JSON.parse(info.out) as { result?: { process_info?: { shell_pid?: number } } }).result?.process_info?.shell_pid ?? 0) : 0;
  const under = shellPid ? descendants(table, shellPid) : [];
  const state = readPaneState(stateDir, pane);
  return { under, state: state && under.some((p) => p.pid === state.pid) ? state : null };
};

/** The pane's omp-dori state, or null when there is none or this pane's agent did not write it. */
export const trustedPaneState = async (run: Runner, stateDir: string, pane: string): Promise<PaneState | null> =>
  readPaneState(stateDir, pane) ? (await paneProcs(run, stateDir, pane, await procTable(run))).state : null;

export type SessionDeps = {
  readonly run: Runner;
  readonly sessionsDir: string;
  /** Where the omp-dori extension writes each pane's state (config.stateDir). */
  readonly stateDir: string;
  readonly listDir?: (dir: string) => readonly string[];
};

/**
 * The session a pane runs. First what the omp-dori extension reported for the pane, when the pane's own agent wrote
 * it; then the agent process under the pane's shell, matched by start time against omp's session files.
 */
export const resolveSession = async (pane: string, cwd: string, table: readonly Proc[], deps: SessionDeps): Promise<SessionHit> => {
  const { under, state } = await paneProcs(deps.run, deps.stateDir, pane, table);
  if (state?.sessionId) return { id: state.sessionId, via: "extension", pid: state.pid };
  const agent = under.find((p) => AGENT.test(p.cmd) && !/\bmcp\b/.test(p.cmd));
  if (!agent) return { id: "", via: "none" };
  const started = Date.parse((await deps.run(["ps", "-o", "lstart=", "-p", String(agent.pid)])).out);
  if (Number.isNaN(started)) return { id: "", via: "none", pid: agent.pid };
  const list = deps.listDir ?? ((d: string) => { try { return readdirSync(d); } catch { return []; } });
  const near = list(sessionDirFor(deps.sessionsDir, cwd)).flatMap((name) => {
    const m = SESSION_FILE.exec(name);
    const at = m ? Date.parse(`${m[1]}:${m[2]}:${m[3]}.${m[4]}Z`) : Number.NaN;
    return m?.[5] && at >= started - 5_000 && at <= started + START_WINDOW_MS ? [{ id: m[5], at }] : [];
  }).sort((a, b) => a.at - b.at);
  return near[0] ? { id: near[0].id, via: "start-time", pid: agent.pid } : { id: "", via: "none", pid: agent.pid };
};
