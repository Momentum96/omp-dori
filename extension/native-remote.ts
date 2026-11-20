import { createHash, randomUUID } from "node:crypto";
import { remoteSocketPath } from "../src/remote/socket-path.ts";
import type { DoriApi, DoriContext } from "./index.ts";
import { NativeRemoteSocket } from "./native-remote-socket.ts";
import { nativeReportLines, remoteMessage, remoteRecord, remoteText, remoteTranscript } from "./native-remote-view.ts";

const PAGE_LIMIT = 40;
const EVENT_WINDOW = 256;
const SEND_LIMIT = 1024;

type Scope = {
  stateDir: string;
  ctx: DoriContext;
  role: "dori" | "lane";
  pane?: string;
  owns: () => boolean;
  approvalPending: () => boolean;
  goal: () => string;
  busy: () => boolean;
};

type Submission = {
  id: string;
  digest: string;
  prompt?: string;
  status: "pending" | "admitted" | "refused";
  preflight?: boolean;
  applying: boolean;
};

type RemoteEvent = { cursor: number; at: string; type: string } & Record<string, unknown>;
type Activation = {
  scope: Scope;
  session: string;
  generation: string;
  resetBoundary: string | undefined;
  events: RemoteEvent[];
  cursor: number;
  submissions: Map<string, Submission>;
  asks: Set<string>;
  reviews: Set<string>;
  awaitingBackground: boolean;
  changingModel: boolean;
  transitioning: boolean;
  closing: boolean;
};

const ok = (data: unknown) => ({ ok: true, data });
const fail = (error: string) => ({ ok: false, error });
const modelId = (model: DoriContext["model"]) =>
  model?.provider ? `${model.provider}/${model.id}` : (model?.id ?? null);
const validId = (id: unknown): id is string => typeof id === "string" && /^[a-zA-Z0-9_.:-]{1,96}$/.test(id);
const resetBoundary = (ctx: DoriContext) =>
  ctx.sessionManager.getBranch().findLast((entry) => entry.type === "reset_boundary")?.id;

const pageSize = (limit: unknown): number | undefined =>
  limit === undefined
    ? 20
    : typeof limit === "number" && Number.isInteger(limit) && limit > 0 && limit <= PAGE_LIMIT
      ? limit
      : undefined;

export class NativeRemote {
  private activation?: Activation;
  private transport?: NativeRemoteSocket;

  constructor(private readonly pi: DoriApi) {}

  async activate(scope: Scope): Promise<void> {
    const session = scope.ctx.sessionManager.getSessionId();
    const path = remoteSocketPath(scope.stateDir, session);
    this.activation = this.newActivation(scope, session);
    if (this.transport?.path === path) {
      this.transport.disconnect();
      return;
    }
    this.transport?.stop();
    const warn = (error: unknown) => {
      if (this.transport !== transport) return;
      this.stop();
      scope.ctx.ui.notify(
        `Native remote unavailable: ${error instanceof Error ? error.message : String(error)}`,
        "warning",
      );
    };
    const transport = new NativeRemoteSocket(path, (request) => this.handle(request), warn);
    this.transport = transport;
    try {
      await transport.start();
    } catch (error) {
      warn(error);
    }
  }

  private newActivation(scope: Scope, session: string, boundary = resetBoundary(scope.ctx)): Activation {
    return {
      scope,
      session,
      generation: randomUUID(),
      resetBoundary: boundary,
      events: [],
      cursor: 0,
      submissions: new Map(),
      asks: new Set(),
      reviews: new Set(),
      awaitingBackground: false,
      changingModel: false,
      transitioning: false,
      closing: false,
    };
  }

  stop(): void {
    this.revoke();
    this.transport?.stop();
    this.transport = undefined;
  }

  revoke(): void {
    this.activation = undefined;
    this.transport?.disconnect();
  }

  /** Prevent changing the target of an in-flight host action. Other transitions revoke the old writer immediately. */
  beforeTransition(ctx: DoriContext): { cancel: true } | undefined {
    const current = this.forContext(ctx);
    if (!current) return;
    if (current.closing || current.changingModel || this.applyingSubmission(current)) {
      ctx.ui.notify("Wait for the current native remote action before changing this session.", "warning");
      return { cancel: true };
    }
    current.transitioning = true;
    this.transport?.disconnect();
  }

  event(type: string, event: unknown, ctx: DoriContext): void {
    const original = this.forContext(ctx);
    const current =
      original && (type === "before_agent_start" || type === "agent_start") ? this.refreshReset(original) : original;
    if (!current || current.transitioning) return;
    const data = remoteRecord(event) ? event : {};
    if (typeof data.sessionId === "string" && data.sessionId !== current.session) return;
    switch (type) {
      case "before_agent_start":
        if (data.reason === "prompt")
          for (const entry of current.submissions.values())
            if (entry.applying && entry.prompt === data.prompt) entry.preflight = true;
        break;
      case "agent_start":
        current.awaitingBackground = false;
        for (const entry of current.submissions.values()) if (entry.preflight) entry.applying = false;
        this.push(current, "agent_start");
        break;
      case "agent_end":
        current.awaitingBackground = data.awaitingAsyncWork === true;
        this.push(current, data.willContinue === true || current.awaitingBackground ? "agent_yield" : "completion", {
          awaitingBackground: current.awaitingBackground,
          ...(ctx.model ? { model: modelId(ctx.model) } : {}),
        });
        break;
      case "turn_start":
      case "turn_end":
        this.push(current, type, { turn: typeof data.turnIndex === "number" ? data.turnIndex : null });
        break;
      case "message_end": {
        const message = remoteMessage(data.message);
        if (message) this.push(current, "message", message);
        break;
      }
      case "tool_execution_start":
        if (data.toolName === "ask" && typeof data.toolCallId === "string") {
          current.asks.add(data.toolCallId);
          this.push(current, "ask", { id: data.toolCallId, status: "pending" });
        }
        break;
      case "tool_execution_end":
        if (data.toolName === "ask" && typeof data.toolCallId === "string" && current.asks.delete(data.toolCallId))
          this.push(current, "ask", { id: data.toolCallId, status: "closed" });
        if (data.toolName === "bash")
          for (const line of nativeReportLines(data))
            this.push(current, "report", { source: "native-tool-result", ...remoteText(line) });
        break;
      case "tool_approval_requested":
      case "tool_approval_resolved":
        if (typeof data.toolCallId === "string")
          this.push(current, "approval", {
            id: data.toolCallId,
            status: type === "tool_approval_requested" ? "pending" : "closed",
          });
        break;
      case "plan_review_requested":
      case "plan_review_resolved":
        if (typeof data.reviewId === "string") {
          const pending = type === "plan_review_requested";
          if (pending) current.reviews.add(data.reviewId);
          else current.reviews.delete(data.reviewId);
          this.push(current, "approval", {
            id: data.reviewId,
            status: pending ? "pending" : "closed",
            kind: "plan-review",
          });
        }
        break;
    }
  }

  private forContext(ctx: DoriContext): Activation | undefined {
    const current = this.activation;
    if (
      current &&
      ctx.agent.kind === "main" &&
      ctx.agent.id === current.scope.ctx.agent.id &&
      ctx.sessionManager.getSessionId() === current.session
    )
      return current;
  }

  private isCurrent(current: Activation, checkReset = true): boolean {
    return (
      this.activation === current &&
      !current.transitioning &&
      current.scope.ctx.sessionManager.getSessionId() === current.session &&
      current.scope.owns() &&
      (!checkReset || resetBoundary(current.scope.ctx) === current.resetBoundary)
    );
  }

  private refreshReset(current: Activation): Activation {
    if (!this.isCurrent(current, false) || current.closing || current.changingModel || this.applyingSubmission(current))
      return current;
    const boundary = resetBoundary(current.scope.ctx);
    if (boundary === current.resetBoundary) return current;
    // Native /clear keeps the session ID and does not emit a session-switch/branch event.
    this.activation = this.newActivation(current.scope, current.session, boundary);
    return this.activation;
  }

  private blocked(current: Activation): boolean {
    return (
      current.scope.approvalPending() ||
      current.asks.size > 0 ||
      current.reviews.size > 0 ||
      current.scope.ctx.getPendingPlanReview?.() !== undefined
    );
  }

  private foreground(current: Activation): boolean {
    const { ctx } = current.scope;
    return current.scope.busy() || ctx.isIdle?.() !== true;
  }

  private background(current: Activation): boolean | null {
    if (current.awaitingBackground) return true;
    const jobs = current.scope.ctx.getAsyncJobSnapshot?.();
    return jobs == null ? null : jobs.running.length > 0 || jobs.delivery.queued > 0 || jobs.delivery.delivering;
  }

  private push(current: Activation, type: string, data: Record<string, unknown> = {}): void {
    current.events.push({ ...data, type, cursor: ++current.cursor, at: new Date().toISOString() });
    if (current.events.length > EVENT_WINDOW) current.events.shift();
  }

  private receipt(entry: Submission) {
    if (entry.status === "refused")
      return ok({ id: entry.id, status: entry.status, reason: "Native OMP did not confirm admission" });
    if (entry.status === "pending") return ok({ id: entry.id, status: entry.status });
    return ok({
      id: entry.id,
      status: entry.status,
      receipt: { id: entry.id, kind: "native-submission" },
      replyAttributed: false,
    });
  }

  private async handle(request: unknown) {
    if (!remoteRecord(request) || typeof request.op !== "string") return fail("Invalid native remote operation");
    const original = this.activation;
    if (!original) return fail("Native remote session is no longer current");
    const current = this.refreshReset(original);
    if (!this.isCurrent(current)) return fail("Native remote session is no longer current");
    const { ctx, pane, role, goal } = current.scope;
    if (request.op === "info") {
      if (request.session !== undefined && request.session !== current.session) return fail("Native session mismatch");
      const sessionFile = ctx.sessionManager.getSessionFile();
      return ok({
        session: current.session,
        generation: current.generation,
        role,
        cwd: ctx.cwd,
        model: modelId(ctx.model),
        status: this.blocked(current)
          ? "blocked"
          : this.foreground(current) || this.background(current) || ctx.hasPendingMessages?.() || current.closing
            ? "working"
            : "idle",
        ...(sessionFile ? { sessionFile } : {}),
        ...(pane ? { pane } : {}),
        goal: goal(),
        pid: process.pid,
      });
    }
    if (request.session !== current.session || request.generation !== current.generation)
      return fail("Native session or generation mismatch");
    if (request.op === "read" || request.op === "events") {
      const limit = pageSize(request.limit);
      if (limit === undefined) return fail(`Native remote limit must be between 1 and ${PAGE_LIMIT}`);
      if (request.op === "read") return ok({ messages: remoteTranscript(ctx.sessionManager.getBranch(), limit) });
      const cursor = request.cursor === undefined ? 0 : request.cursor;
      if (typeof cursor !== "number" || !Number.isSafeInteger(cursor) || cursor < 0 || cursor > current.cursor)
        return fail("Invalid native remote event cursor");
      const events = current.events.filter((event) => event.cursor > cursor).slice(0, limit);
      return ok({
        events,
        cursor: events.at(-1)?.cursor ?? cursor,
        gap: cursor + 1 < (current.events[0]?.cursor ?? 1),
      });
    }
    if (pane?.startsWith("headless:")) return fail("Headless mutations must use the native command supervisor");
    if (request.op === "receipt") {
      if (!validId(request.id)) return fail("Native receipt requires a request id");
      const entry = current.submissions.get(request.id);
      return entry ? this.receipt(entry) : ok({ id: request.id, status: "not-found" });
    }
    if (current.closing) return fail("Native shutdown has already been requested");
    if (request.op === "send") {
      const { id, text, steer = false } = request;
      if (
        !validId(id) ||
        typeof text !== "string" ||
        !text.trim() ||
        Buffer.byteLength(text) > 32 * 1024 ||
        typeof steer !== "boolean"
      )
        return fail("Native send requires a request id, text of at most 32 KiB, and optional boolean steer");
      const digest = createHash("sha256")
        .update(steer ? "1" : "0")
        .update(text)
        .digest("hex");
      const existing = current.submissions.get(id);
      if (existing)
        return existing.digest === digest ? this.receipt(existing) : fail("Native request ID has different input");
      if (this.blocked(current)) return fail("The owner must resolve the local approval or ASK");
      if (current.changingModel || this.applyingSubmission(current))
        return fail("Another native submission or model change is being applied");
      const foreground = this.foreground(current);
      if (steer ? !foreground : foreground || ctx.hasPendingMessages?.())
        return fail(
          steer ? "No foreground turn is available to steer" : "Native session is busy; choose explicit steer",
        );
      if (goal() === "paused" || goal() === "blocked") return fail("The owner must resume the native goal locally");
      if (current.submissions.size >= SEND_LIMIT)
        return fail("Native submission receipt capacity reached; re-activate the session to submit new IDs");
      const entry: Submission = { id, digest, prompt: text, status: "pending", applying: true };
      current.submissions.set(id, entry);
      try {
        const native = this.pi.sendUserMessage(text, { deliverAs: steer ? "steer" : "aside" });
        void native.then(
          () => this.settle(current, entry, "admitted"),
          () => this.settle(current, entry, "refused"),
        );
      } catch {
        this.settle(current, entry, "refused");
      }
      return this.receipt(entry);
    }
    if (this.blocked(current)) return fail("The owner must resolve the local approval or ASK");
    if (current.changingModel) return fail("A native model change is being applied");
    if (request.op === "interrupt") {
      if (!this.foreground(current) && !this.applyingSubmission(current) && ctx.hasPendingMessages?.() !== true)
        return ok({ status: "idle", background: "not-targeted", backgroundPending: this.background(current) });
      const backgroundPending = this.background(current);
      this.requestAbort(current);
      return ok({ status: "requested", background: "not-targeted", backgroundPending });
    }
    if (request.op === "model") {
      if (typeof request.model !== "string" || !request.model.trim() || request.model.length > 256)
        return fail("Native model requires a model selector");
      if (this.busy(current)) return fail("Native model changes require an idle session");
      if (!ctx.models || !this.pi.setModel) return fail("This OMP host does not expose native model changes");
      const chosen = ctx.models.resolve(request.model);
      if (!chosen) return fail("OMP could not resolve the requested model");
      const expected = modelId(chosen);
      if (!this.isCurrent(current) || this.blocked(current)) return fail("Native session is no longer writable");
      if (modelId(ctx.models.current()) === expected) return ok({ model: expected });
      current.changingModel = true;
      try {
        if (!(await this.pi.setModel(chosen))) return fail("OMP did not accept the model");
        if (!this.isCurrent(current) || this.blocked(current))
          return fail("Native model readback could not be confirmed");
        const selected = modelId(ctx.models.current());
        if (selected !== expected) return fail("Native model readback does not match the requested model");
        this.push(current, "model", { model: selected });
        return ok({ model: selected });
      } catch {
        return fail("OMP did not confirm the requested model");
      } finally {
        current.changingModel = false;
      }
    }
    if (request.op === "close") {
      if (request.force !== undefined && typeof request.force !== "boolean")
        return fail("Native close force must be boolean");
      if (!ctx.shutdown) return fail("This OMP host does not expose graceful shutdown");
      const busy = this.busy(current);
      if (busy && request.force !== true) return fail("Native session is busy; close requires idle or explicit force");
      current.closing = true;
      try {
        if (this.foreground(current) || this.applyingSubmission(current) || ctx.hasPendingMessages?.())
          this.requestAbort(current);
        if (!this.isCurrent(current) || this.blocked(current)) {
          current.closing = false;
          return fail("Native shutdown was not requested because the session is no longer writable");
        }
        ctx.shutdown();
        return ok({ status: "requested", background: "not-targeted" });
      } catch {
        current.closing = false;
        return ok({ status: "close-unconfirmed", background: "not-targeted" });
      }
    }
    return fail("Unknown native remote operation");
  }

  private busy(current: Activation): boolean {
    return (
      this.foreground(current) ||
      current.scope.ctx.hasPendingMessages?.() === true ||
      this.background(current) === true ||
      this.applyingSubmission(current)
    );
  }

  private applyingSubmission(current: Activation): boolean {
    return [...current.submissions.values()].some((entry) => entry.applying);
  }

  private settle(current: Activation, entry: Submission, status: "admitted" | "refused"): void {
    entry.applying = false;
    entry.prompt = undefined;
    if (!this.isCurrent(current)) return;
    entry.status = status;
    this.push(current, "submission", { id: entry.id, status, replyAttributed: false });
  }

  private requestAbort(current: Activation): void {
    const result = current.scope.ctx.abort();
    void Promise.resolve(result).catch(() => {
      if (this.isCurrent(current)) this.push(current, "interrupt", { status: "unconfirmed" });
    });
  }
}
