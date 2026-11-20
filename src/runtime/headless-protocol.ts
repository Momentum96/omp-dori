import type { AgentStatus, DeliverOptions, Probe, SessionRef } from "./types.ts";
import type { HeadlessRemoteRequest } from "./headless-remote.ts";

export type HeadlessGoal = {
  readonly status: "none" | "active" | "paused" | "complete" | "dropped" | "blocked";
  readonly objective?: string;
};
export type HeadlessQuestion = {
  readonly id: string;
  readonly question: string;
  readonly options: readonly string[];
  readonly multi: boolean;
};
export type HeadlessDialog = {
  readonly id: string;
  readonly kind: "approval" | "confirm" | "select" | "input" | "editor" | "ask" | "unsupported";
  readonly title?: string;
  readonly message?: string;
  readonly options?: readonly string[];
  readonly questions?: readonly HeadlessQuestion[];
  readonly secret?: true;
};
export type HeadlessAnswer =
  | { readonly cancelled: true }
  | { readonly confirmed: boolean }
  | { readonly value: string }
  | {
      readonly answers: readonly {
        readonly id: string;
        readonly selectedOptions: readonly string[];
        readonly customInput?: string;
      }[];
    };
export type HeadlessEntry = { readonly kind: "assistant" | "output" | "notice"; readonly text: string };
export type HeadlessObservation = {
  readonly ref: SessionRef;
  readonly cwd: string;
  readonly startedAt: string;
  readonly updatedAt: string;
  readonly status: AgentStatus;
  readonly pid?: number;
  readonly role?: "lane";
  readonly sessionId?: string;
  readonly sessionFile?: string;
  readonly goal?: HeadlessGoal;
  readonly model?: string;
  readonly held: boolean;
  readonly dialogs: readonly HeadlessDialog[];
  readonly recent: readonly HeadlessEntry[];
  readonly activeSince?: number;
  readonly detail?: string;
};

export const headlessProbe = (seen: HeadlessObservation): Probe => ({
  status: seen.status,
  shellInFront: false,
  approvalPending: seen.held,
  cwd: seen.cwd,
  ...(seen.role ? { agent: "omp" } : {}),
  ...(seen.pid !== undefined ? { pid: seen.pid, rootPid: seen.pid } : {}),
  ...(seen.sessionId !== undefined ? { sessionId: seen.sessionId } : {}),
  ...(seen.sessionFile !== undefined ? { sessionFile: seen.sessionFile } : {}),
});

export const headlessRead = (seen: HeadlessObservation, lines: number): string => {
  if (lines <= 0) return "";
  const out = [
    ...seen.recent.map((entry) => entry.text),
    ...seen.dialogs.map((dialog) => {
      const detail = dialog.title ?? dialog.questions?.map((question) => question.question).join("; ") ?? "";
      return `[${dialog.kind} ${dialog.id}] ${Bun.stripANSI(detail)}`;
    }),
  ];
  return out.join("\n").split("\n").slice(-lines).join("\n");
};

export type HeadlessRequest =
  | HeadlessRemoteRequest
  | { readonly op: "observe" }
  | { readonly op: "begin"; readonly expectedSessionId: string; readonly objective: string }
  | { readonly op: "deliver"; readonly text: string; readonly options: DeliverOptions }
  | { readonly op: "goal"; readonly expectedSessionId: string; readonly target: "paused" | "active" }
  | {
      readonly op: "answer";
      readonly expectedSessionId: string;
      readonly requestId: string;
      readonly answer: HeadlessAnswer;
    }
  | { readonly op: "close"; readonly expectedSessionId: string };

export const object = (value: unknown): Record<string, unknown> | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : undefined;
export const string = (value: unknown): string | undefined => (typeof value === "string" ? value : undefined);
export const strings = (value: unknown): string[] =>
  Array.isArray(value) ? value.filter((entry): entry is string => typeof entry === "string") : [];

export const modelOf = (value: unknown): string | undefined => {
  const model = object(value);
  const provider = string(model?.["provider"]);
  const id = string(model?.["id"]);
  return provider && id ? `${provider}/${id}` : undefined;
};

export const entryOf = (frame: Record<string, unknown>): HeadlessEntry | undefined => {
  const message = object(frame["message"]);
  let kind: HeadlessEntry["kind"];
  let text: unknown;
  if (frame["type"] === "message_end" && message?.["role"] === "assistant" && Array.isArray(message["content"])) {
    kind = "assistant";
    text = message["content"]
      .flatMap((part: unknown) => {
        const item = object(part);
        return item?.["type"] === "text" && typeof item["text"] === "string" ? [item["text"]] : [];
      })
      .join("");
  } else if (frame["type"] === "command_output") {
    kind = "output";
    text = frame["text"];
  } else if (frame["type"] === "extension_ui_request" && frame["method"] === "notify") {
    kind = "notice";
    text = frame["message"];
  } else return undefined;
  if (typeof text !== "string" || !text.trim()) return undefined;
  return { kind, text: text.replace(/\p{Cc}/gu, (c) => (c === "\n" || c === "\t" ? c : "")).slice(-2_000) };
};

export const goalOf = (value: unknown): HeadlessGoal | undefined => {
  if (value === null) return { status: "none" };
  const goal = object(object(value)?.["goal"]);
  const status = goal?.["status"];
  const objective = string(goal?.["objective"]);
  switch (status) {
    case "active":
    case "paused":
    case "complete":
    case "dropped":
      return { status, ...(objective !== undefined ? { objective } : {}) };
    case "budget-limited":
      return { status: "blocked", ...(objective !== undefined ? { objective } : {}) };
    default:
      return undefined;
  }
};

export const dialogOf = (frame: Record<string, unknown>): HeadlessDialog | undefined => {
  const id = string(frame["id"]);
  if (!id) return undefined;
  const title = string(frame["title"]);
  const message = string(frame["message"]);
  const options = strings(frame["options"]);
  const summary = { id, ...(title !== undefined ? { title } : {}), ...(message !== undefined ? { message } : {}) };
  switch (frame["method"]) {
    case "select": {
      // Pinned OMP tools/approval.ts uses this select fallback, including Deny and the single-call approval.
      const approval = options.includes("Deny") && (options.includes("Approve") || options.includes("Approve Once"));
      return { ...summary, kind: approval ? "approval" : "select", options };
    }
    case "confirm":
      return { ...summary, kind: "confirm" };
    case "input":
      return { ...summary, kind: "input", ...(frame["secret"] === true ? { secret: true } : {}) };
    case "editor":
      return { ...summary, kind: "editor" };
    case "ask": {
      const questions = Array.isArray(frame["questions"])
        ? frame["questions"].flatMap((item): HeadlessQuestion[] => {
            const question = object(item);
            const qid = string(question?.["id"]);
            const text = string(question?.["question"]);
            if (!question || !qid || text === undefined) return [];
            const choices = Array.isArray(question["options"])
              ? question["options"].flatMap((item): string[] => {
                  const label = string(object(item)?.["label"]);
                  return label === undefined ? [] : [label];
                })
              : [];
            return [{ id: qid, question: text, options: choices, multi: question["multi"] === true }];
          })
        : [];
      return questions.length ? { id, kind: "ask", questions } : { id, kind: "unsupported" };
    }
    case "cancel":
    case "notify":
    case "setStatus":
    case "setWidget":
    case "setTitle":
    case "set_editor_text":
      return undefined;
    default:
      // Unknown UI (including open_url) may be gated. Do not expose its URLs or implicitly respond.
      return { id, kind: "unsupported" };
  }
};

/** Never forward unrecognized fields (particularly timedOut, which may submit recommended answers). */
export const answerFrame = (dialog: HeadlessDialog, answer: unknown): Record<string, unknown> | undefined => {
  const response = object(answer);
  if (!response || Object.keys(response).length !== 1) return undefined;
  const frame = { type: "extension_ui_response", id: dialog.id };
  if (dialog.kind !== "unsupported" && response["cancelled"] === true) return { ...frame, cancelled: true };
  if (dialog.secret || dialog.kind === "unsupported") return undefined;
  if (dialog.kind === "confirm" && typeof response["confirmed"] === "boolean")
    return { ...frame, confirmed: response["confirmed"] };
  const value = string(response["value"]);
  if (dialog.kind === "input" || dialog.kind === "editor") return value === undefined ? undefined : { ...frame, value };
  if (dialog.kind === "approval" || dialog.kind === "select") {
    if (value === undefined || !dialog.options?.includes(value)) return undefined;
    // No session/global grants. Approving one named pending call must not approve future calls too.
    if (dialog.kind === "approval" && value !== "Deny" && value !== "Approve Once" && value !== "Approve")
      return undefined;
    return { ...frame, value };
  }
  const answers = response["answers"];
  const questions = dialog.questions;
  if (!questions || !Array.isArray(answers) || answers.length !== questions.length) return undefined;
  const valid: Record<string, unknown>[] = [];
  for (const [index, question] of questions.entries()) {
    const answer = object(answers[index]);
    const selected = answer?.["selectedOptions"];
    const custom = answer?.["customInput"];
    if (
      answer?.["id"] !== question.id ||
      !Array.isArray(selected) ||
      selected.some((option) => typeof option !== "string" || !question.options.includes(option)) ||
      new Set(selected).size !== selected.length ||
      (custom !== undefined && typeof custom !== "string") ||
      (!question.multi && selected.length + (custom?.trim() ? 1 : 0) > 1) ||
      (selected.length === 0 && !custom?.trim())
    )
      return undefined;
    valid.push({
      id: question.id,
      selectedOptions: selected,
      ...(custom === undefined ? {} : { customInput: custom }),
    });
  }
  return { ...frame, answers: valid };
};
