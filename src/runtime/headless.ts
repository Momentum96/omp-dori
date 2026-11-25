import { randomUUID } from "node:crypto";
import { mkdirSync, realpathSync } from "node:fs";
import { join, resolve } from "node:path";
import { guardConsumerReservation } from "../release/launch-leases.ts";
import type { HeadlessBeginOutcome } from "./headless-launch-model.ts";
import { closeHost, reapGoneHost, startHost, stopHarnessRole } from "./headless-lifecycle.ts";
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
  type CloseScope,
  type CreateSpec,
  type DeliverOptions,
  type DeliverOutcome,
  HEADLESS_ADDRESS,
  type NativeSessionRuntime,
  type Probe,
  type SessionInfo,
  type SessionRef,
  type SessionRole,
} from "./types.ts";

export type { HeadlessAnswer, HeadlessObservation } from "./headless-protocol.ts";
export { headlessArgv } from "./headless-argv.ts";
type Env = Readonly<Record<string, string | undefined>>;

/** The same target-local runtime works from a fresh CLI process; only its detached supervisor owns the OMP stdin. */
export class HeadlessRuntime implements NativeSessionRuntime {
  readonly kind = "headless" as const;
  readonly stateDir: string;

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

  async create(spec: CreateSpec): Promise<SessionRef> {
    if (process.platform === "win32")
      throw new host.HeadlessError("headless sessions require a Unix socket (macOS or Linux)");
    const ref = { runtime: this.kind, id: randomUUID(), ...(spec.group === undefined ? {} : { group: spec.group }) };
    const cwd = realpathSync(spec.cwd);
    const base = host.dataDir(this.stateDir);
    const unlock = guardConsumerReservation(resolve(import.meta.dir, "../.."), this.stateDir);
    try {
      mkdirSync(base, { recursive: true, mode: 0o700 });
      host.privateDir(base);
      const dir = host.laneDir(this.stateDir, ref);
      mkdirSync(dir, { mode: 0o700 });
      host.writePrivate(join(dir, "reservation.json"), {
        ref,
        cwd,
        name: spec.name,
        role: spec.role,
        startedAt: new Date().toISOString(),
      });
      return ref;
    } finally {
      unlock();
    }
  }

  async startNative(
    ref: SessionRef,
    argv: readonly string[],
    overrides: Env,
    resume?: { readonly file: string; readonly sessionId: string },
  ): Promise<HeadlessObservation> {
    if (!argv.length) throw new host.HeadlessError("native OMP requires an executable");
    return startHost(this.stateDir, ref, argv, { ...this.env, ...overrides }, resume);
  }

  startLoop(ref: SessionRef, overrides: Env): Promise<HeadlessObservation> {
    return startHost(this.stateDir, ref, [], { ...this.env, ...overrides });
  }

  loopLog(ref: SessionRef): string {
    return join(host.laneDir(this.stateDir, ref), "loop.log");
  }

  stopHarnessRole(ref: SessionRef, role: Exclude<SessionRole, "lane">, sessionId?: string): Promise<void> {
    return stopHarnessRole(this.stateDir, ref, role, sessionId);
  }

  reapGoneHost(ref: SessionRef, role: SessionRole): boolean {
    return reapGoneHost(this.stateDir, ref, role);
  }

  observe(ref: SessionRef): Promise<HeadlessObservation> {
    return host.observeHost(this.stateDir, ref);
  }

  async begin(ref: SessionRef, expectedSessionId: string, objective: string): Promise<HeadlessBeginOutcome> {
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
      return await this.call(ref, {
        op: "deliver",
        text,
        options: { ...options, expectedSessionId: options.expectedSessionId ?? current.sessionId },
      });
    } catch (failure) {
      if (failure instanceof host.HostTransportError)
        return { kind: "ambiguous", detail: "headless host response lost after delivery attempt", failure };
      return { kind: "not-ready", status: "unknown" };
    }
  }

  close(ref: SessionRef, _scope?: CloseScope, expectedSessionId?: string): Promise<void> {
    return closeHost(this.stateDir, ref, expectedSessionId);
  }

  private call<T>(ref: SessionRef, request: HeadlessRequest): Promise<T> {
    return host.requestHost<T>(this.stateDir, ref, request);
  }
}
