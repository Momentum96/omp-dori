import {
  answerFrame,
  dialogOf,
  entryOf,
  goalOf,
  type HeadlessDialog,
  type HeadlessEntry,
  type HeadlessObservation,
  type HeadlessRequest,
  modelOf,
  string,
} from "./headless-protocol.ts";
import { type HostStart, type Reservation, serveHost } from "./headless-record.ts";
import { deliverLocal, HeadlessRemoteCommands, isRemoteCommand, type NativeState } from "./headless-remote.ts";
import { createNativeGoal, type HeadlessRpc, proveLaneRole, setNativeGoal, startRpc } from "./headless-rpc.ts";
import type { AgentStatus } from "./types.ts";

type Wire = Record<string, unknown>;
const RING_SIZE = 40;

class Broker {
  private readonly dialogs = new Map<string, HeadlessDialog>();
  private readonly recent: HeadlessEntry[] = [];
  private readonly remote = new HeadlessRemoteCommands();
  private bound: NativeState | undefined;
  private state: NativeState | undefined;
  private stateRead: Promise<NativeState | undefined> | undefined;
  private rpc: HeadlessRpc | undefined;
  private detail: string | undefined;
  private activeSince: number | undefined;
  private mutation = false;
  private stopping = false;
  private fault = false;
  closedSessionId: string | undefined;

  constructor(private readonly reservation: Reservation) {}

  async start(start: HostStart, overlay: string): Promise<number> {
    const { cwd, ref } = this.reservation;
    const rpc = await startRpc(
      start,
      cwd,
      ref.id,
      overlay,
      (frame) => this.event(frame),
      () => {
        this.fault = true;
        if (!this.stopping) this.detail = "OMP RPC stream disconnected or became untrustworthy";
      },
    );
    this.rpc = rpc;
    try {
      this.bound = await proveLaneRole(rpc, () => this.refresh(), start.expectedResumeId);
      return await rpc.exited;
    } catch (error) {
      this.detail = error instanceof Error ? error.message : String(error);
      // A stalled native exit is still live; do not publish a stopped receipt while it owns the session.
      await rpc.shutdown().catch(() => rpc.exited);
      throw error;
    }
  }

  private event(frame: Wire): void {
    switch (frame["type"]) {
      case "extension_ui_request": {
        if (frame["method"] === "cancel") {
          const target = string(frame["targetId"]);
          if (target) this.dialogs.delete(target);
          break;
        }
        const dialog = dialogOf(frame);
        if (dialog) this.dialogs.set(dialog.id, dialog);
        break;
      }
      case "agent_start":
        this.activeSince ??= Date.now();
        break;
      case "session_settled":
        this.activeSince = undefined;
        break;
    }
    const entry = entryOf(frame);
    if (entry) {
      this.recent.push(entry);
      if (this.recent.length > RING_SIZE) this.recent.shift();
    }
  }

  private async refresh(): Promise<NativeState | undefined> {
    if (!this.rpc || this.fault || this.stopping) return undefined;
    this.stateRead ??= (async () => {
      const data = await this.rpc?.request({ type: "get_state" });
      const sessionId = string(data?.["sessionId"]);
      const sessionFile = string(data?.["sessionFile"]);
      const goal = goalOf(data?.["goal"]);
      const settled = data?.["isSettled"];
      if (!sessionId || !sessionFile || !goal || typeof settled !== "boolean") return undefined;
      if (this.bound && (this.bound.sessionId !== sessionId || this.bound.sessionFile !== sessionFile)) {
        this.fault = true;
        this.detail = "OMP changed its bound transcript";
        return undefined;
      }
      const foreground = data?.["isStreaming"] === true || data?.["isCompacting"] === true;
      const model = modelOf(data?.["model"]);
      this.state = { sessionId, sessionFile, goal, settled, foreground, ...(model === undefined ? {} : { model }) };
      if (settled) this.activeSince = undefined;
      return this.state;
    })().finally(() => {
      this.stateRead = undefined;
    });
    return this.stateRead;
  }

  private snapshot(state: NativeState | undefined): HeadlessObservation {
    const { ref, cwd, startedAt } = this.reservation;
    const held = this.dialogs.size > 0 || this.fault || this.stopping || !state || !this.bound;
    const status: AgentStatus = this.fault
      ? "error"
      : this.dialogs.size > 0 || state?.goal.status === "blocked"
        ? "blocked"
        : !state || !this.bound || this.stopping
          ? "unknown"
          : state.settled
            ? "idle"
            : "working";
    return {
      ref,
      cwd,
      startedAt,
      updatedAt: new Date().toISOString(),
      status,
      held,
      ...(this.rpc ? { pid: this.rpc.pid } : {}),
      ...(this.bound && !this.fault && this.state
        ? {
            role: "lane",
            sessionId: this.bound.sessionId,
            sessionFile: this.bound.sessionFile,
            goal: this.state.goal,
            ...(this.state.model === undefined ? {} : { model: this.state.model }),
          }
        : {}),
      ...(this.activeSince !== undefined ? { activeSince: this.activeSince } : {}),
      ...(this.detail !== undefined ? { detail: this.detail } : {}),
      dialogs: [...this.dialogs.values()],
      recent: this.recent.slice(),
    };
  }

  private async observe(): Promise<HeadlessObservation> {
    // Reading during a dialog or initial handshake does not queue behind it. No mutation or answer is issued here.
    if (!this.bound || this.dialogs.size || this.stopping)
      return this.snapshot(this.dialogs.size ? this.state : undefined);
    return this.snapshot(await this.refresh().catch(() => undefined));
  }

  private async check(expected: string | undefined): Promise<NativeState> {
    if (
      !this.bound ||
      this.dialogs.size ||
      this.fault ||
      this.stopping ||
      (expected !== undefined && expected !== this.bound.sessionId)
    )
      throw new Error("native identity or actionable dialog holds this operation");
    const state = await this.refresh();
    if (!state || this.dialogs.size || this.fault || this.stopping)
      throw new Error("native identity or actionable dialog holds this operation");
    return state;
  }

  async handle(request: HeadlessRequest): Promise<unknown> {
    if (request.op === "observe") return this.observe();
    const rpc = this.rpc;
    if (!rpc) throw new Error("OMP process has not started");
    if (isRemoteCommand(request))
      return this.remote.handle(request, {
        rpc,
        boundSessionId: this.fault ? undefined : this.bound?.sessionId,
        check: (session) => this.check(session),
        exclusive: (operation) => this.exclusive(operation),
        shutdown: (session) => this.shutdown(rpc, session),
      });
    if (request.op === "answer") {
      if (this.fault || this.stopping || !this.bound || request.expectedSessionId !== this.bound.sessionId)
        throw new Error("native dialog session does not match");
      const state = await this.refresh().catch(() => undefined);
      if (!state || this.fault || this.stopping) throw new Error("native dialog session readback unconfirmed");
      const dialog = this.dialogs.get(request.requestId);
      const frame = dialog && answerFrame(dialog, request.answer);
      if (!dialog || !frame) throw new Error("no matching native dialog or unsupported answer");
      // No answer replay after attempting the write; RPC UI replies do not have an ack.
      this.dialogs.delete(dialog.id);
      try {
        rpc.send(frame);
      } catch {
        this.fault = true;
        throw new Error("native answer delivery ambiguous; do not resend");
      }
      return { submitted: true };
    }
    return this.exclusive(async () => {
      switch (request.op) {
        case "deliver": {
          const state = await this.check(request.options.expectedSessionId).catch(() => undefined);
          return state
            ? deliverLocal(rpc, state, request.text, request.options)
            : { kind: "not-ready", status: this.dialogs.size ? "blocked" : "unknown" };
        }
        case "begin": {
          await createNativeGoal(rpc, await this.check(request.expectedSessionId), request.objective);
          return await this.observe();
        }
        case "goal": {
          await setNativeGoal(rpc, request.target, () => this.check(request.expectedSessionId));
          return await this.observe();
        }
        case "close": {
          const state = await this.check(request.expectedSessionId);
          if (!state.settled) throw new Error("only a settled OMP lane can close");
          await this.shutdown(rpc, state.sessionId);
          return { closed: true };
        }
        default:
          throw new Error("Unsupported headless operation");
      }
    });
  }

  private async exclusive<T>(operation: () => Promise<T>): Promise<T> {
    if (this.mutation) throw new Error("another headless operation is in flight");
    this.mutation = true;
    try {
      return await operation();
    } finally {
      this.mutation = false;
    }
  }

  private async shutdown(rpc: HeadlessRpc, session: string): Promise<void> {
    this.stopping = true;
    this.closedSessionId = session;
    await rpc.shutdown();
  }
}

if (import.meta.main) {
  const dir = process.argv[2];
  if (dir) await serveHost(dir, (reservation) => new Broker(reservation));
}
