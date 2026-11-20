import { createHash } from "node:crypto";
import { realpathSync, statSync } from "node:fs";
import { basename, isAbsolute, relative } from "node:path";
import { liveSessions, nativeRequest, object, type Request, type Session } from "../../../../src/remote/client.ts";
import { hcCall } from "../../../../src/runtime/hc/exec.ts";
import { createArgv, slugName } from "../../../../src/runtime/hc/sessions.ts";
import { isOmpArgv } from "../../../../src/runtime/omp-argv.ts";
import type { SessionRef } from "../../../../src/runtime/types.ts";
import { configPath, expandHome, fill } from "./config.ts";
import type { FlowDeps } from "./done-flow.ts";
import { resumeFile } from "./lifecycle-session.ts";
import { statusOf } from "./registry.ts";
import { remoteId, RemoteRecords } from "./remote-records.ts";
import { runtimesOf } from "./run.ts";
import { laneLaunchEnv } from "./slack-token.ts";

function executable(deps: FlowDeps): readonly string[] {
  const argv = deps.config.agentCommand;
  const role = argv.indexOf("--dori-role");
  const model = argv.indexOf("--model");
  if (
    !isOmpArgv(argv) ||
    role < 1 ||
    argv[role + 1] !== "lane" ||
    model !== role + 2 ||
    argv[model + 1] !== "{model}" ||
    argv[model + 2] !== "{prompt}" ||
    model + 3 !== argv.length
  )
    throw new Error("Remote launch needs Cataly's native OMP lane argv; custom startup commands are not run remotely");
  return argv.slice(0, role);
}

async function nativeCli(deps: FlowDeps, argv: readonly string[]) {
  const result = await deps.run([...executable(deps), ...argv], { timeoutMs: 15_000 });
  if (result.code !== 0) throw new Error(`OMP ${argv[0]} failed (${result.code})`);
  return result.out;
}

function directory(path: string): string {
  const expanded = expandHome(path);
  if (!isAbsolute(expanded)) throw new Error("Use an absolute target-machine directory");
  const real = realpathSync(expanded);
  if (!statSync(real).isDirectory()) throw new Error("Target path is not a directory");
  return real;
}

async function waitForSession(deps: FlowDeps, ref: SessionRef, cwd: string, expected?: string): Promise<Session> {
  const runtime = runtimesOf(deps).for(ref);
  const end = deps.clock.now() + 20_000;
  let identity = "no live OMP endpoint";
  do {
    const probe = await runtime.probe(ref, { foregroundOnly: true });
    if (probe.status === "gone") break;
    const live = (await liveSessions(deps.config.stateDir)).find((row) => row.pane === ref.id);
    if (live?.pid) {
      // A role-scoped private endpoint comes from the agent itself, not text printed in a pane. Bind it to the actual
      // foreground PID from hc; attestation for legacy terminal typing remains separate and unchanged.
      const foreground = probe.foreground?.some((proc) => proc.pid === live.pid);
      identity = `pane=${probe.status}, foreground=${Boolean(foreground)}, shell=${probe.shellInFront}`;
      if (
        foreground &&
        !probe.shellInFront &&
        (!expected || live.session === expected) &&
        realpathSync(live.cwd) === cwd
      )
        return live;
    }
    await deps.clock.sleep(200);
  } while (deps.clock.now() < end);
  throw new Error(`OMP startup unconfirmed in ${ref.id} (${identity}); inspect it before attempting another launch`);
}

async function start(deps: FlowDeps, cwd: string, name: string, argv: readonly string[], expected?: string) {
  const argv0 = argv[0];
  if (!argv0) throw new Error("Missing native OMP executable");
  const create = createArgv(
    { name, cwd, env: laneLaunchEnv(process.env, configPath(), deps.config), role: "lane", container: "tab" },
    argv0,
    slugName(name),
    process.env["DORI_HC_SPACE"],
  );
  // A standalone session has no shell flow to preserve. Let hc exec the OMP argv directly.
  const result = await hcCall(
    async (cmd, args) => {
      const child = await deps.run([cmd, ...args], { timeoutMs: 10_000 });
      return { code: child.code, stdout: child.out, stderr: child.err };
    },
    [...create, ...argv.slice(1), `--cwd=${cwd}`],
  );
  const address = result["address"];
  if (typeof address !== "string" || !/^t[0-9a-z]+$/.test(address))
    throw new Error("hc did not confirm a new local terminal");
  const target = { runtime: "hc", id: address } as const;
  return await waitForSession(deps, target, cwd, expected);
}

export async function openRemoteSession(deps: FlowDeps, request: Request): Promise<unknown> {
  const id = request.id;
  if (!id || !remoteId.test(id)) throw new Error("Open requires a request ID");
  const records = new RemoteRecords(deps.config.stateDir);
  return await records.exclusive(`open-${id}`, async () => {
    const receipt = records.path("open", id);
    const prior = records.read(receipt);
    const fingerprint = createHash("sha256")
      .update(
        JSON.stringify([
          request.cwd,
          request.name,
          request.model,
          request.thinking,
          request.worktree,
          request.branch,
          request.base,
          request.text,
        ]),
      )
      .digest("hex");
    if (prior && prior["fingerprint"] !== fingerprint)
      throw new Error("Open request ID was already used with different input");
    if (prior?.["status"] === "opened") {
      const { fingerprint: _fingerprint, ...result } = prior;
      return result;
    }
    if (prior) throw new Error("Previous open is unconfirmed; inspect the target before attempting another launch");
    const cwd = directory(request.cwd ?? deps.config.defaultCwd);
    const model = request.model ?? deps.config.defaultModel;
    if (!model || model.startsWith("-") || model.includes("\0")) throw new Error("Invalid OMP model");
    if (request.thinking && !/^(off|minimal|low|medium|high|xhigh|max|auto)$/.test(request.thinking))
      throw new Error("Invalid OMP thinking level");
    const argv = [
      ...fill(deps.config.agentCommand, { model }).slice(0, -1),
      ...(request.thinking ? [`--thinking=${request.thinking}`] : []),
    ];
    executable(deps);
    if (!request.worktree && (request.branch || request.base))
      throw new Error("--branch and --base require --worktree");
    if (request.worktree && !isAbsolute(expandHome(request.worktree))) throw new Error("Use an absolute worktree path");
    if (request.base?.startsWith("-") || request.branch?.startsWith("-")) throw new Error("Invalid Git reference");
    records.store(receipt, { status: "pending", fingerprint });
    const worktree = request.worktree ? expandHome(request.worktree) : undefined;
    if (worktree)
      await nativeCli(deps, [
        "worktree",
        "add",
        `--cwd=${cwd}`,
        ...(request.branch ? [`--branch=${request.branch}`] : []),
        "--",
        worktree,
        ...(request.base ? [request.base] : []),
      ]);
    const actualCwd = worktree ? directory(worktree) : cwd;
    const session = await start(deps, actualCwd, request.name ?? `remote-${id.slice(0, 8)}`, argv);
    records.store(records.path("session", session.session), {
      cwd: actualCwd,
      sessionFile: session.sessionFile,
      model: session.model,
    });
    const opened = { ...session, status: "opened", worktree: worktree ?? null, id };
    records.store(receipt, {
      ...opened,
      fingerprint,
      ...(request.text ? { delivery: { id, status: "unconfirmed" } } : {}),
    });
    if (!request.text) return opened;
    try {
      const delivery = await nativeRequest(deps.config.stateDir, {
        op: "send",
        session: session.session,
        generation: session.generation,
        id,
        text: request.text,
      });
      const result = { ...opened, delivery };
      records.store(receipt, { ...result, fingerprint });
      return result;
    } catch (error) {
      const result = { ...opened, deliveryError: error instanceof Error ? error.message : String(error) };
      records.store(receipt, { ...result, fingerprint });
      return result;
    }
  });
}

export async function wakeRemoteSession(deps: FlowDeps, id: string): Promise<Session> {
  const records = new RemoteRecords(deps.config.stateDir);
  const path = records.path("session", id);
  return await records.exclusive(`session-${id}`, async () => {
    const saved = records.read(path);
    if (saved?.["closed"]) throw new Error("Explicitly closed OMP sessions cannot be woken remotely");
    if (saved?.["closePending"]) throw new Error("Remote close outcome is unconfirmed; refusing cold wake");
    const live = (await liveSessions(deps.config.stateDir)).find((row) => row.session === id);
    if (live) return live;
    const lane = (await deps.registry.list()).find((row) => row.session === id);
    if (lane && (statusOf(lane) === "closed" || lane.nativeCloseIntent || lane.hold))
      throw new Error("Closed or held lanes require explicit local reopen");
    // Registered lanes have their own registry/ref migration contract; never spawn an unbound copy.
    if (lane) throw new Error("Registered lanes must be reconciled with Cataly's native reopen command");
    const file = saved?.["sessionFile"];
    const cwd = saved?.["cwd"];
    if (typeof file !== "string" || typeof cwd !== "string") throw new Error("No eligible saved Cataly remote session");
    const root = realpathSync(deps.config.sessionsDir);
    const exact = await resumeFile(deps, { key: id, session: id, sessionFile: file, cwd });
    const under = relative(root, exact);
    if (!under || under.startsWith("..") || isAbsolute(under) || !basename(exact).endsWith(".jsonl"))
      throw new Error("Saved session is outside the configured OMP sessions directory");
    const lease = object(JSON.parse(await nativeCli(deps, ["session", "lease", "--json", "--", id])));
    if (
      !Array.isArray(lease?.["held"]) ||
      !Array.isArray(lease["unknown"]) ||
      lease["held"].length ||
      lease["unknown"].length
    )
      throw new Error("OMP cannot confirm that the saved session is unowned");
    return await start(
      deps,
      directory(cwd),
      `remote-${id.slice(0, 8)}`,
      [...executable(deps), "--dori-role", "lane", `--resume=${exact}`],
      id,
    );
  });
}
