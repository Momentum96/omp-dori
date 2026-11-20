import { randomUUID } from "node:crypto";
import { isOmpArgv } from "./omp-argv.ts";
import { object } from "./headless-protocol.ts";
import type { HostStart } from "./headless-record.ts";
import type { NativeState } from "./headless-remote.ts";

type Wire = Record<string, unknown>;
const MAX_FRAME = 1024 * 1024;
const RPC_TIMEOUT = 15_000;
export class RpcRefusal extends Error {}

const FORBIDDEN_FLAGS =
  /^(?:--(?:mode|no-ui|no-session|session-dir|session|resume|continue|restore|fork|from-claude|from-codex|provider-session-id|goal|approval-mode|auto-approve|yolo|print)|-[crp].*|--)(?:=|$)/;

/** Headless uses exact native argv, but never accepts a caller-supplied mode or transcript selector. */
export const headlessArgv = (template: readonly string[], model: string): string[] => {
  if (template.at(-1) !== "{prompt}" || template.slice(0, -1).some((arg) => arg.includes("{prompt}")))
    throw new Error("headless agentCommand must end in one positional {prompt}");
  const argv = template.slice(0, -1).map((arg) => arg.replaceAll("{model}", () => model));
  if (!isOmpArgv(argv) || argv.some((arg) => FORBIDDEN_FLAGS.test(arg)))
    throw new Error(
      "headless lanes require a native OMP agentCommand without mode, session, goal or approval overrides",
    );
  const at = argv.findIndex((arg) => arg === "--dori-role" || arg.startsWith("--dori-role="));
  if (at === -1) argv.push("--dori-role", "lane");
  else if (
    !["--dori-role=lane", "--dori-role"].includes(argv[at] ?? "") ||
    (argv[at] === "--dori-role" && argv[at + 1] !== "lane") ||
    argv.slice(at + 1).some((arg) => /^--dori-role(?:=|$)/.test(arg))
  )
    throw new Error("headless agentCommand may only select the Dori lane role");
  return argv;
};

export async function startRpc(
  start: HostStart,
  cwd: string,
  id: string,
  overlay: string,
  event: (frame: Wire) => void,
  fault: () => void,
): Promise<HeadlessRpc> {
  const env = { ...start.env, CATALY_HEADLESS_ID: id, PI_NOTIFICATIONS: "off" };
  const version = Bun.spawn([...start.argv, "--version"], {
    cwd,
    env,
    stdout: "pipe",
    stderr: "ignore",
    stdin: "ignore",
    timeout: RPC_TIMEOUT,
  });
  const [printed, code] = await Promise.all([new Response(version.stdout).text(), version.exited]);
  if (code !== 0 || !/^(?:omp\s+)?18\.20\.0$/im.test(printed.trim()))
    throw new Error("headless lanes require OMP 18.20.0");
  return new HeadlessRpc(
    [...start.argv, "--mode", "rpc-ui", "--approval-mode", "always-ask", "--config", overlay],
    { cwd, env },
    event,
    fault,
  );
}

export async function proveLaneRole(
  rpc: HeadlessRpc,
  readState: () => Promise<NativeState | undefined>,
  expected: string | undefined,
): Promise<NativeState> {
  await rpc.waitReady();
  await rpc.request({ type: "set_ask_dialog", enabled: true });
  const before = await readState();
  const entries = (await rpc.request({ type: "get_entries" }))?.["entries"];
  const roles = Array.isArray(entries)
    ? entries.map(object).filter((entry) => entry?.["type"] === "custom" && entry["customType"] === "omp-dori:role")
    : [];
  const current = await readState();
  if (
    roles.at(-1)?.["data"] !== "lane" ||
    !before ||
    !current ||
    before.sessionId !== current.sessionId ||
    before.sessionFile !== current.sessionFile ||
    (expected !== undefined && expected !== current.sessionId)
  )
    throw new Error("OMP did not prove a stable persisted session with the Dori lane role");
  return current;
}

export async function setNativeGoal(
  rpc: HeadlessRpc,
  target: "paused" | "active",
  check: () => Promise<NativeState>,
): Promise<void> {
  const state = await check();
  if (state.goal.status === "blocked") throw new Error("native goal requires direct inspection");
  const terminal = state.goal.status === "none" || state.goal.status === "complete" || state.goal.status === "dropped";
  if (!terminal && state.goal.status !== target)
    await rpc.request({ type: "goal", op: target === "paused" ? "pause" : "resume" });
  const after = await check();
  if (!terminal && after.goal.status !== target) throw new Error("native goal change was not confirmed");
  if (target === "paused" && !after.settled) {
    await rpc.request({ type: "abort" });
    if (!(await check()).settled) throw new Error("native turn interruption was not confirmed");
  }
}

export async function createNativeGoal(rpc: HeadlessRpc, state: NativeState, objective: string): Promise<void> {
  if (!state.settled || state.goal.status !== "none" || !objective.trim())
    throw new Error("a fresh settled OMP lane is required to create a goal");
  const result = await rpc.request({ type: "goal", op: "create", objective });
  if (object(result?.["goal"])?.["status"] !== "active") throw new Error("OMP did not acknowledge an active goal");
}

/** This client uses pinned protocol v1: JSONL, one command/response id, and separate native UI events. */
export class HeadlessRpc {
  private readonly pending = new Map<string, { readonly command: string; readonly done: (frame?: Wire) => void }>();
  private readonly ready = Promise.withResolvers<boolean>();
  private stopped = false;
  private nativeReady = false;
  readonly exited: Promise<number>;
  readonly pid: number;
  private readonly child: Bun.Subprocess<"pipe", "pipe", "ignore">;

  constructor(
    argv: readonly string[],
    opts: { readonly cwd: string; readonly env: Record<string, string | undefined> },
    private readonly onEvent: (frame: Wire) => void,
    private readonly onFault: () => void,
  ) {
    this.child = Bun.spawn([...argv], { ...opts, stdin: "pipe", stdout: "pipe", stderr: "ignore" });
    this.pid = this.child.pid;
    this.exited = this.child.exited.finally(() => this.end());
    void this.read().catch(() => this.end());
  }

  async waitReady(): Promise<void> {
    const timer = setTimeout(() => this.ready.resolve(false), RPC_TIMEOUT);
    try {
      if (!(await this.ready.promise)) throw new Error("OMP RPC handshake did not complete");
    } finally {
      clearTimeout(timer);
    }
  }

  async request(command: Wire & { readonly type: string }): Promise<Wire | undefined> {
    const id = randomUUID();
    const reply = Promise.withResolvers<Wire | undefined>();
    this.pending.set(id, { command: command.type, done: reply.resolve });
    const timer = setTimeout(() => reply.resolve(), RPC_TIMEOUT);
    try {
      this.send({ ...command, id });
      const frame = await reply.promise;
      if (!frame) throw new Error(`OMP RPC ${command.type} response was not confirmed`);
      if (frame["success"] !== true) throw new RpcRefusal(`OMP rejected RPC ${command.type}`);
      return object(frame["data"]);
    } finally {
      clearTimeout(timer);
      this.pending.delete(id);
    }
  }

  send(frame: Wire): void {
    if (this.stopped || !this.nativeReady) throw new Error("OMP RPC stream unavailable");
    const data = `${JSON.stringify(frame)}\n`;
    if (Buffer.byteLength(data) > MAX_FRAME) throw new Error("OMP RPC frame is too large");
    // Once we attempt to write a mutation, disconnect means ambiguous. Do not replay it.
    const sink = this.child.stdin;
    const count = sink.write(data);
    if (count !== Buffer.byteLength(data)) throw new Error("OMP RPC write was not confirmed");
    sink.flush();
  }

  async shutdown(): Promise<void> {
    // EOF triggers the pinned OMP RPC teardown: session shutdown, dialog cancellation and awaited disposal.
    this.child.stdin.end();
    const stopped = await Promise.race([this.exited.then(() => true), Bun.sleep(RPC_TIMEOUT).then(() => false)]);
    if (!stopped) throw new Error("OMP did not confirm exit after RPC disconnect");
  }

  private end(): void {
    if (this.stopped) return;
    this.stopped = true;
    this.ready.resolve(false);
    for (const { done } of this.pending.values()) done();
    this.onFault();
  }

  private async read(): Promise<void> {
    const decoder = new TextDecoder();
    let rest = "";
    for await (const chunk of this.child.stdout) {
      rest += decoder.decode(chunk, { stream: true });
      for (let at = rest.indexOf("\n"); at !== -1; at = rest.indexOf("\n")) {
        const line = rest.slice(0, at);
        rest = rest.slice(at + 1);
        if (!line.trim()) continue;
        if (Buffer.byteLength(line) > MAX_FRAME) throw new Error("oversized OMP RPC frame");
        const frame = object(JSON.parse(line));
        if (!frame) throw new Error("invalid OMP RPC frame");
        this.handle(frame);
      }
      if (Buffer.byteLength(rest) > MAX_FRAME) throw new Error("oversized OMP RPC frame");
    }
    this.end();
  }

  private handle(frame: Wire): void {
    if (frame["type"] === "ready" && frame["protocolVersion"] === 1 && !this.nativeReady) {
      this.nativeReady = true;
      this.ready.resolve(true);
      return;
    }
    if (!this.nativeReady) throw new Error("OMP did not speak pinned RPC protocol");
    if (frame["type"] === "response") {
      const id = frame["id"];
      const pending = typeof id === "string" ? this.pending.get(id) : undefined;
      if (pending && pending.command === frame["command"]) pending.done(frame);
    } else if (frame["type"] === "rpc_frame_error") {
      // A dropped native UI frame could otherwise leave a falsely idle or sendable observation.
      throw new Error("OMP dropped an RPC frame");
    } else {
      this.onEvent(frame);
    }
  }
}
