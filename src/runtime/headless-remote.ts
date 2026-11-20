import { createHash } from "node:crypto";
import type { HeadlessGoal } from "./headless-protocol.ts";
import { type HeadlessRpc, RpcRefusal } from "./headless-rpc.ts";
import type { DeliverOptions, DeliverOutcome } from "./types.ts";

export type HeadlessRemoteInput = {
  readonly id: string;
  readonly text: string;
  readonly delivery?: "prompt" | "steer";
};
export type HeadlessRemoteReceipt = {
  readonly id: string;
  readonly status: "pending" | "admitted" | "refused";
  readonly replyAttributed: false;
  readonly receipt?: { readonly id: string; readonly kind: "native-submission" };
  readonly reason?: string;
};
export type HeadlessRemoteInterrupt = {
  readonly status: "interrupted" | "refused";
  readonly foreground: boolean;
  readonly background: "unverified";
  readonly reason?: string;
};
export type HeadlessRemoteModel = {
  readonly status: "changed" | "refused";
  readonly model?: string;
  readonly reason?: string;
};
export type HeadlessRemoteClose = {
  readonly status: "closed" | "refused" | "close-unconfirmed";
  readonly reason?: string;
};
export type HeadlessRemoteRequest = { readonly expectedSessionId: string } & (
  | { readonly op: "remote-send"; readonly input: HeadlessRemoteInput }
  | { readonly op: "remote-receipt"; readonly id: string }
  | { readonly op: "remote-interrupt" }
  | { readonly op: "remote-model"; readonly model: string }
  | { readonly op: "remote-close"; readonly force: boolean }
);

export type NativeState = {
  readonly sessionId: string;
  readonly sessionFile: string;
  readonly goal: HeadlessGoal;
  readonly settled: boolean;
  readonly foreground: boolean;
  readonly model?: string;
};

type Gate = {
  readonly rpc: HeadlessRpc;
  readonly boundSessionId: string | undefined;
  readonly check: (session: string) => Promise<NativeState>;
  readonly exclusive: <T>(operation: () => Promise<T>) => Promise<T>;
  readonly shutdown: (session: string) => Promise<void>;
};
type Submission = { readonly digest: string; receipt: HeadlessRemoteReceipt };
const MAX_RECEIPTS = 512; // Never evict an earlier ID into a potentially duplicated submission.
const validId = (id: unknown): id is string => typeof id === "string" && /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/.test(id);
const why = (error: unknown): string => (error instanceof Error ? error.message : String(error));

export const isRemoteCommand = (request: { readonly op: string }): request is HeadlessRemoteRequest =>
  ["remote-send", "remote-receipt", "remote-interrupt", "remote-model", "remote-close"].includes(request.op);

export async function deliverLocal(
  rpc: HeadlessRpc,
  state: NativeState,
  text: string,
  options: DeliverOptions,
): Promise<DeliverOutcome> {
  const held = state.goal.status === "paused" || state.goal.status === "blocked";
  const status = held ? "blocked" : state.settled ? "idle" : "working";
  const when = options.strictIdle ? "idle" : (options.when ?? "idle");
  if (held || (when !== "any" && status !== when) || !text.trim()) return { kind: "not-ready", status };
  try {
    // Preserve the local lane's established behavior; remote exact input never wraps slash-leading text.
    const message = text.trimStart().startsWith("/") ? `[CATALY message]\n${text}` : text;
    await rpc.request({ type: "follow_up", message });
    return { kind: "delivered", started: state.settled };
  } catch {
    return { kind: "ambiguous", detail: "OMP did not confirm the submitted input", failure: null };
  }
}

/** Admission receipts live exactly as long as this process-bound native lane. They never correlate a model reply. */
export class HeadlessRemoteCommands {
  private readonly submissions = new Map<string, Submission>();

  async handle(request: HeadlessRemoteRequest, gate: Gate): Promise<unknown> {
    const { expectedSessionId: session } = request;
    if (!gate.boundSessionId || session !== gate.boundSessionId) throw new Error("Native session identity mismatch");
    switch (request.op) {
      case "remote-send":
        return this.send(gate, session, request.input);
      case "remote-receipt":
        if (!validId(request.id)) throw new Error("Native receipt requires a request ID");
        return this.submissions.get(request.id)?.receipt;
      case "remote-interrupt":
        return this.interrupt(gate, session);
      case "remote-model":
        return this.model(gate, session, request.model);
      case "remote-close":
        return this.close(gate, session, request.force);
    }
  }

  private async send(gate: Gate, session: string, input: HeadlessRemoteInput): Promise<HeadlessRemoteReceipt> {
    const { id, text, delivery = "prompt" } = input;
    if (
      !validId(id) ||
      typeof text !== "string" ||
      !text.trim() ||
      Buffer.byteLength(text) > 32 * 1024 ||
      (delivery !== "prompt" && delivery !== "steer")
    )
      throw new Error("Native send requires a request ID, at most 32 KiB of text, and prompt or steer delivery");
    const digest = createHash("sha256").update(`${delivery}\0`).update(text).digest("hex");
    const prior = this.submissions.get(id);
    if (prior) {
      if (prior.digest !== digest) throw new Error("Native request ID has different input");
      return prior.receipt;
    }
    if (this.submissions.size >= MAX_RECEIPTS) throw new Error("Native submission receipt storage is full");
    const result = (status: HeadlessRemoteReceipt["status"], reason?: string): HeadlessRemoteReceipt => ({
      id,
      status,
      replyAttributed: false,
      ...(status === "admitted" ? { receipt: { id, kind: "native-submission" } } : {}),
      ...(reason === undefined ? {} : { reason }),
    });
    const entry: Submission = { digest, receipt: result("pending") };
    this.submissions.set(id, entry);
    try {
      await gate.exclusive(async () => {
        const state = await gate.check(session);
        if (state.goal.status === "paused" || state.goal.status === "blocked")
          throw new Error("The owner must resume the native goal locally");
        if (delivery === "steer" ? !state.foreground : !state.settled)
          throw new Error(
            delivery === "steer" ? "No foreground turn to steer" : "Native lane is busy; choose explicit steer",
          );
        // 18.20.0 has no raw flag for steer/follow_up. It will expand a slash-leading template or reject an extension
        // command, so refuse this form rather than changing the payload or invoking a command.
        if (text.trimStart().startsWith("/"))
          throw new Error("Native headless RPC cannot submit slash-leading text literally");
        try {
          await gate.rpc.request({ type: delivery === "steer" ? "steer" : "follow_up", message: text });
          entry.receipt = result("admitted");
        } catch (error) {
          entry.receipt =
            error instanceof RpcRefusal
              ? result("refused", "OMP did not acknowledge admission")
              : result("pending", "Admission unconfirmed; do not submit a new ID");
        }
      });
    } catch (error) {
      entry.receipt = result("refused", why(error));
    }
    return entry.receipt;
  }

  private interrupt(gate: Gate, session: string): Promise<HeadlessRemoteInterrupt> {
    const background = "unverified" as const;
    return gate.exclusive(async () => {
      const state = await gate.check(session).catch(() => undefined);
      if (!state?.foreground)
        return {
          status: "refused",
          foreground: false,
          background,
          reason: state
            ? "No foreground turn to interrupt"
            : "Native identity or actionable dialog holds the operation",
        };
      await gate.rpc.request({ type: "abort" });
      const after = await gate.check(session).catch(() => undefined);
      if (!after || after.foreground) throw new Error("Native abort requested; foreground readback unconfirmed");
      return { status: "interrupted", foreground: true, background };
    });
  }

  private model(gate: Gate, session: string, model: string): Promise<HeadlessRemoteModel> {
    const pair = typeof model === "string" && model.length <= 256 ? /^([^/\s]+)\/(\S+)$/.exec(model) : null;
    if (!pair) return Promise.resolve({ status: "refused", reason: "Model must be an exact provider/id" });
    return gate.exclusive(async () => {
      const state = await gate.check(session).catch(() => undefined);
      if (!state?.settled)
        return { status: "refused", reason: "Native model changes require a settled, unblocked lane" };
      if (state.model === model) return { status: "changed", model };
      try {
        await gate.rpc.request({ type: "set_model", provider: pair[1], modelId: pair[2] });
      } catch (error) {
        if (error instanceof RpcRefusal) return { status: "refused", reason: "OMP rejected the model selector" };
        throw new Error("Native model request unconfirmed; inspect the current model");
      }
      if ((await gate.check(session)).model !== model) throw new Error("Native model readback did not match");
      return { status: "changed", model };
    });
  }

  private close(gate: Gate, session: string, force: boolean): Promise<HeadlessRemoteClose> {
    if (typeof force !== "boolean") return Promise.resolve({ status: "refused", reason: "Invalid close force flag" });
    return gate.exclusive(async () => {
      const state = await gate.check(session).catch(() => undefined);
      // RPC disconnect cancels dialogs. While a turn or background work can present one, a previous
      // state read cannot authorize forced EOF: the next approval frame may be in transit.
      if (!state?.settled)
        return {
          status: "refused",
          reason: state
            ? "Native lane must settle before close, even with force"
            : "Native identity or actionable dialog holds the operation",
        };
      try {
        await gate.shutdown(session);
        return { status: "closed" };
      } catch {
        return { status: "close-unconfirmed", reason: "OMP exit did not confirm after graceful RPC disconnect" };
      }
    });
  }
}
