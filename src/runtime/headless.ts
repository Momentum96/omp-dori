import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, realpathSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import {
  type HeadlessAnswer,
  type HeadlessObservation,
  type HeadlessRequest,
  headlessProbe,
  headlessRead,
} from "./headless-protocol.ts";
import * as host from "./headless-record.ts";
import type {
  HeadlessRemoteClose,
  HeadlessRemoteInput,
  HeadlessRemoteInterrupt,
  HeadlessRemoteModel,
  HeadlessRemoteReceipt,
} from "./headless-remote.ts";
import {
  type AgentIdentity,
  type CloseScope,
  type CreateSpec,
  type DeliverOptions,
  type DeliverOutcome,
  HEADLESS_ADDRESS,
  type Key,
  type Probe,
  type SessionInfo,
  type SessionRef,
  type SessionRuntime,
  type SubmitResult,
} from "./types.ts";

export type { HeadlessAnswer, HeadlessObservation } from "./headless-protocol.ts";
export { headlessArgv } from "./headless-rpc.ts";
type Env = Readonly<Record<string, string | undefined>>;
const WAIT_FOR_HOST = 65_000;

/** The same target-local runtime works from a fresh CLI process; only its detached supervisor owns the OMP stdin. */
export class HeadlessRuntime implements SessionRuntime {
  readonly kind = "headless" as const;
  private readonly stateDir: string;

  constructor(
    stateDir: string,
    private readonly env: Env = process.env,
  ) {
    this.stateDir = resolve(stateDir);
  }

  self(): SessionRef | undefined {
    const id = this.env["CATALY_HEADLESS_ID"];
    return id && HEADLESS_ADDRESS.test(id) ? { runtime: this.kind, id } : undefined;
  }

  async createGroup(): Promise<string> {
    throw new host.HeadlessError("headless OMP lanes have no terminal workspace");
  }

  async create(spec: CreateSpec): Promise<SessionRef> {
    if (process.platform === "win32")
      throw new host.HeadlessError("headless OMP lanes require a Unix socket (macOS or Linux)");
    if (spec.role !== "lane") throw new host.HeadlessError("headless OMP is only available for lanes");
    const ref = { runtime: this.kind, id: randomUUID(), ...(spec.group === undefined ? {} : { group: spec.group }) };
    const cwd = realpathSync(spec.cwd);
    const base = host.dataDir(this.stateDir);
    mkdirSync(base, { recursive: true, mode: 0o700 });
    host.privateDir(base);
    const dir = host.laneDir(this.stateDir, ref);
    mkdirSync(dir, { mode: 0o700 });
    host.writePrivate(join(dir, "reservation.json"), {
      ref,
      cwd,
      name: spec.name,
      startedAt: new Date().toISOString(),
    });
    return ref;
  }

  async start(): Promise<void> {
    throw new host.HeadlessError("headless OMP does not accept terminal command strings");
  }

  async startNative(
    ref: SessionRef,
    argv: readonly string[],
    overrides: Env,
    resume?: { readonly file: string; readonly sessionId: string },
  ): Promise<HeadlessObservation> {
    const entry = host.laneEntry(this.stateDir, ref);
    if (!entry) throw new host.HeadlessError("headless lane reservation missing");
    const { dir } = entry;
    if (existsSync(join(dir, "stopped.json"))) throw new host.HeadlessError("headless lane reservation has stopped");
    // A competing start/close gets the same exclusive file. There is no recovery by retrying a spawn after ambiguity.
    writeFileSync(join(dir, "started"), "", { flag: "wx", mode: 0o600 });
    const env = { ...this.env, ...overrides };
    for (const key of [
      "HC_TERMINAL_ID",
      "HERDR_PANE_ID",
      "HERDR_WORKSPACE_ID",
      "HERDR_TAB_ID",
      "CATALY_HEADLESS_ID",
      "CATALY_ASK_LAUNCHED_LANE",
    ])
      delete env[key];
    const child = await host.spawnHost(dir, {
      argv: resume ? [...argv, `--resume=${resume.file}`] : argv,
      env,
      ...(resume ? { expectedResumeId: resume.sessionId } : {}),
    });
    const deadline = Date.now() + WAIT_FOR_HOST;
    do {
      const observed = await this.observe(ref);
      if (observed.role === "lane" && observed.sessionId && observed.sessionFile) return observed;
      if (observed.status === "gone" || child.exitCode !== null || child.signalCode !== null)
        throw new host.HeadlessError(observed.detail ?? "native OMP host exited before proving its lane session");
      await Bun.sleep(50);
    } while (Date.now() < deadline);
    throw new host.HeadlessError("native OMP host did not prove its lane session before the startup deadline");
  }

  observe(ref: SessionRef): Promise<HeadlessObservation> {
    return host.observeHost(this.stateDir, ref);
  }

  async begin(ref: SessionRef, expectedSessionId: string, objective: string): Promise<HeadlessObservation> {
    return this.call(ref, { op: "begin", expectedSessionId, objective });
  }

  async setGoal(ref: SessionRef, expectedSessionId: string, target: "paused" | "active"): Promise<HeadlessObservation> {
    return this.call(ref, { op: "goal", expectedSessionId, target });
  }

  async answer(
    ref: SessionRef,
    requestId: string,
    answer: HeadlessAnswer,
    expectedSessionId: string,
  ): Promise<{ readonly submitted: true }> {
    return this.call(ref, { op: "answer", expectedSessionId, requestId, answer });
  }

  async remoteSend(
    ref: SessionRef,
    expectedSessionId: string,
    input: HeadlessRemoteInput,
  ): Promise<HeadlessRemoteReceipt> {
    try {
      return await this.call(ref, { op: "remote-send", expectedSessionId, input });
    } catch (error) {
      if (!(error instanceof host.HostTransportError)) throw error;
      return {
        id: input.id,
        status: "pending",
        replyAttributed: false,
        reason: "Admission unconfirmed; inspect the receipt",
      };
    }
  }

  remoteReceipt(ref: SessionRef, expectedSessionId: string, id: string): Promise<HeadlessRemoteReceipt | undefined> {
    return this.call(ref, { op: "remote-receipt", expectedSessionId, id });
  }

  remoteInterrupt(ref: SessionRef, expectedSessionId: string): Promise<HeadlessRemoteInterrupt> {
    return this.call(ref, { op: "remote-interrupt", expectedSessionId });
  }

  remoteModel(ref: SessionRef, expectedSessionId: string, model: string): Promise<HeadlessRemoteModel> {
    return this.call(ref, { op: "remote-model", expectedSessionId, model });
  }

  async remoteClose(ref: SessionRef, expectedSessionId: string, force = false): Promise<HeadlessRemoteClose> {
    const stopped = () => host.readPrivate<host.StopReceipt>(join(host.laneDir(this.stateDir, ref), "stopped.json"));
    if (stopped()?.closedSessionId === expectedSessionId) return { status: "closed" };
    try {
      return await this.call(ref, { op: "remote-close", expectedSessionId, force });
    } catch (error) {
      if (!(error instanceof host.HostTransportError)) throw error;
      return stopped()?.closedSessionId === expectedSessionId
        ? { status: "closed" }
        : { status: "close-unconfirmed", reason: "Could not confirm exit of the bound OMP lane" };
    }
  }

  list(filter?: { readonly group?: string }): Promise<readonly SessionInfo[]> {
    return host.listHosts(this.stateDir, filter?.group);
  }

  async probe(ref: SessionRef): Promise<Probe> {
    return headlessProbe(await this.observe(ref));
  }

  async read(ref: SessionRef, lines: number): Promise<string> {
    return headlessRead(await this.observe(ref), lines);
  }

  async deliver(ref: SessionRef, text: string, options: DeliverOptions = {}): Promise<DeliverOutcome> {
    if (Buffer.byteLength(text) > 256 * 1024) return { kind: "not-ready", status: "unknown" };
    const current = await this.observe(ref);
    if (current.status === "gone") return { kind: "no-agent" };
    if (current.held || !current.sessionId) return { kind: "not-ready", status: current.status };
    try {
      return await this.call(ref, { op: "deliver", text, options });
    } catch (failure) {
      if (failure instanceof host.HostTransportError)
        return { kind: "ambiguous", detail: "headless host response lost after delivery attempt", failure };
      return { kind: "not-ready", status: "unknown" };
    }
  }

  async submitTyped(_ref: SessionRef, _into: AgentIdentity): Promise<SubmitResult> {
    return "changed";
  }

  async typeIntoShell(): Promise<boolean> {
    return false;
  }

  async keys(_ref: SessionRef, _keys: readonly Key[]): Promise<void> {
    throw new host.HeadlessError("headless OMP has no terminal keys; use the native lifecycle operation");
  }

  async close(ref: SessionRef, _scope?: CloseScope, expectedSessionId?: string): Promise<void> {
    const entry = host.laneEntry(this.stateDir, ref);
    if (!entry || host.readPrivate<host.StopReceipt>(join(entry.dir, "stopped.json"))) return;
    if (!existsSync(join(entry.dir, "started"))) {
      // Competing start either already owns this record or will see that close reserved it.
      writeFileSync(join(entry.dir, "started"), "", { flag: "wx", mode: 0o600 });
      host.writePrivate(join(entry.dir, "stopped.json"), {});
      return;
    }
    const observed = await this.observe(ref);
    if (observed.status === "gone") return;
    const bound = expectedSessionId ?? observed.sessionId;
    if (!bound) throw new host.HeadlessError("headless close has no confirmed native session identity");
    await this.call(ref, { op: "close", expectedSessionId: bound }).catch((error: unknown) => {
      if (
        !(error instanceof host.HostTransportError) ||
        !host.readPrivate<host.StopReceipt>(join(entry.dir, "stopped.json"))
      )
        throw error;
    });
    if ((await this.observe(ref)).status !== "gone") throw new host.HeadlessError("headless close not confirmed");
  }

  private call<T>(ref: SessionRef, request: HeadlessRequest): Promise<T> {
    return host.requestHost<T>(this.stateDir, ref, request);
  }
}
