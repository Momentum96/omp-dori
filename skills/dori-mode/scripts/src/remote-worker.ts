import { liveSessions, nativeRequest, object, selectSession, type Request } from "../../../../src/remote/client.ts";
import { remotePolicy } from "../../../../src/remote/policy.ts";
import { readRequest } from "../../../../src/remote/transport.ts";
import { HeadlessRuntime } from "../../../../src/runtime/headless.ts";
import { parseRef } from "../../../../src/runtime/types.ts";
import type { FlowDeps } from "./done-flow.ts";
import { alive } from "./checkpoint-lock.ts";
import { openRemoteSession, wakeRemoteSession } from "./remote-launch.ts";
import { RemoteRecords } from "./remote-records.ts";
import { runtimesOf } from "./run.ts";

const OPERATIONS = new Set([
  "sessions",
  "info",
  "read",
  "events",
  "send",
  "receipt",
  "interrupt",
  "model",
  "close",
  "attach",
  "open",
  "wake",
]);

export async function remoteOperation(deps: FlowDeps, request: Request, peer: boolean): Promise<unknown> {
  const { stateDir } = deps.config;
  if (!OPERATIONS.has(request.op)) throw new Error("Unsupported Cataly remote operation");
  const policy = remotePolicy(stateDir);
  if (peer && !policy.control) throw new Error("Remote control is disabled on this Cataly installation");
  if (request.op === "open") return await openRemoteSession(deps, request);
  if (request.op === "sessions") {
    const lanes = await deps.registry.list();
    return (await liveSessions(stateDir)).map((row) => ({
      ...row,
      lane: lanes.find((lane) => lane.session === row.session)?.key ?? null,
    }));
  }
  if (!request.session || request.session.length > 128) throw new Error("An OMP session ID is required");
  if (request.op === "wake") {
    if (!policy.wake) throw new Error("Session wake is disabled on this Cataly installation");
    return await wakeRemoteSession(deps, request.session);
  }
  const target = selectSession(await liveSessions(stateDir), request.session);
  if (request.generation !== undefined && request.generation !== target.generation)
    throw new Error("OMP session generation changed; inspect its current state first");
  if (request.op === "info") return target;
  if (request.op === "attach") {
    if (!target.pane || !/^t[0-9a-z]+$/.test(target.pane))
      return { mode: "transcript", session: target.session, generation: target.generation };
    // Also attest the pane's process, not just the numeric terminal ID in metadata.
    const ref = { runtime: "hc", id: target.pane } as const;
    const bound = await runtimesOf(deps).for(ref).probe(ref);
    if (!target.pid || !bound.foreground?.some((proc) => proc.pid === target.pid) || bound.shellInFront)
      throw new Error("The terminal no longer owns the requested native OMP session");
    if (!bound.rootPid) throw new Error("Could not verify the target terminal process");
    return { terminal: target.pane, rootPid: bound.rootPid, session: target.session };
  }
  const headless = target.pane ? parseRef(target.pane) : undefined;
  if (headless?.runtime === "headless" && !["read", "events"].includes(request.op)) {
    const runtime = runtimesOf(deps).for(headless);
    if (!(runtime instanceof HeadlessRuntime)) throw new Error("Headless supervisor unavailable");
    const id = request.id;
    const mutation = async () => {
      switch (request.op) {
        case "send":
          if (!id || !request.text) throw new Error("Native send needs an ID and text");
          return await runtime.remoteSend(headless, target.session, {
            id,
            text: request.text,
            delivery: request.steer ? "steer" : "prompt",
          });
        case "receipt":
          if (!id) throw new Error("Native receipt needs an ID");
          return (await runtime.remoteReceipt(headless, target.session, id)) ?? { id, status: "not-found" };
        case "interrupt":
          return await runtime.remoteInterrupt(headless, target.session);
        case "model":
          if (!request.model) throw new Error("Native model change needs a model");
          return await runtime.remoteModel(headless, target.session, request.model);
        case "close":
          return await runtime.remoteClose(headless, target.session, request.force);
        default:
          throw new Error("Unsupported headless remote operation");
      }
    };
    const result = await (request.op === "close"
      ? new RemoteRecords(stateDir).close(target.session, mutation)
      : mutation());
    return { ...result, session: target.session, generation: target.generation };
  }
  const call = () => nativeRequest(stateDir, { ...request, session: target.session, generation: target.generation });
  if (request.op === "close")
    return await new RemoteRecords(stateDir).close(target.session, async () => {
      await call();
      for (let check = 0; check < 20; check++) {
        const visible = (await liveSessions(stateDir)).find((row) => row.session === target.session);
        if (!visible && target.pid !== undefined && !alive(target.pid))
          return { status: "closed", session: target.session };
        if (visible && visible.generation !== target.generation) break;
        await deps.clock.sleep(100);
      }
      return { status: "close-unconfirmed", session: target.session };
    });
  const result = await call();
  if (request.op === "send") return { ...object(result), session: target.session, generation: target.generation };
  return result;
}

export async function acceptRemote(deps: FlowDeps): Promise<void> {
  try {
    // Reject before reading peer-controlled payloads while disabled; check again at execution.
    if (!remotePolicy(deps.config.stateDir).control)
      throw new Error("Remote control is disabled on this Cataly installation");
    const request = await readRequest(process.stdin);
    const data = await remoteOperation(deps, request, true);
    console.log(JSON.stringify({ ok: true, data }));
  } catch (error) {
    console.log(JSON.stringify({ ok: false, error: error instanceof Error ? error.message : String(error) }));
  }
}
