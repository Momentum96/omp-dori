import { createInterface } from "node:readline";
import { appendFileSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";

if (process.argv.includes("--version")) {
  console.log(process.env["CATALY_TEST_OMP_VERSION"] ?? "18.20.0");
  process.exit(0);
}
if (!process.argv.includes("rpc-ui") || !process.argv.includes("always-ask") || process.env["HERDR_PANE_ID"])
  throw new Error("not the isolated native headless launch");

const saved = process.env["CATALY_TEST_SESSION"]!;
const trace = process.env["CATALY_TEST_TRACE"]!;
const resume = process.argv.find((arg) => arg.startsWith("--resume="));
const id = resume ? (JSON.parse(readFileSync(saved, "utf8").trim()) as { id: string }).id : randomUUID();
if (!resume) writeFileSync(saved, `${JSON.stringify({ type: "session", id, cwd: process.cwd() })}\n`);
const role = process.argv.includes("lane") ? "lane" : "dori";
const send = (data: object) => process.stdout.write(`${JSON.stringify(data)}\n`);
let working = false;
let background = false;
let model = { provider: "fixture", id: "initial" };
let goal: { status: string; objective: string } | null = resume ? { status: "paused", objective: "restored" } : null;
let prompt: string | null = null;
send({ type: "ready", protocolVersion: 1, supportedProtocolVersions: [1, 2] });
for await (const line of createInterface({ input: process.stdin })) {
  const frame = JSON.parse(line) as Record<string, unknown>;
  appendFileSync(trace, `${line}\n`);
  const type = frame["type"];
  const ack = (data?: object) => send({ id: frame["id"], type: "response", command: type, success: true, data });
  if (type === "get_state") {
    const done = process.env["CATALY_TEST_BACKGROUND_DONE"];
    if (done && existsSync(done)) background = false;
    ack({ sessionId: id, sessionFile: saved, isSettled: !working && !background, isStreaming: working,
      goal: goal ? { goal } : null, model });
  } else if (type === "get_entries") {
    ack({ entries: [{ type: "custom", customType: "omp-dori:role", data: role }] });
  } else if (type === "goal") {
    if (frame["op"] === "create") goal = { status: "active", objective: String(frame["objective"]) };
    if (frame["op"] === "pause" && goal) goal.status = "paused";
    if (frame["op"] === "resume" && goal) goal.status = "active";
    ack({ goal });
  } else if (type === "abort") {
    working = false;
    send({ type: "session_settled" });
    ack();
  } else if (type === "set_ask_dialog") {
    ack({ enabled: true });
  } else if (type === "set_model") {
    model = { provider: String(frame["provider"]), id: String(frame["modelId"]) };
    ack(model);
  } else if (type === "follow_up" || type === "steer") {
    const message = String(frame["message"]);
    if (message.startsWith("slow admission")) await Bun.sleep(300);
    ack();
    if (message === "long turn") {
      working = true;
      send({ type: "agent_start" });
    } else if (message === "background job") {
      background = true;
    } else if (message === "request approval" || message === "request question") {
      working = true;
      send({ type: "agent_start" });
      prompt = randomUUID();
      send(message === "request approval"
        ? {
            type: "extension_ui_request",
            id: prompt,
            method: "select",
            title: "Approve this fake write?",
            options: ["Approve Once", "Approve for this session", "Deny"],
          }
        : {
            type: "extension_ui_request",
            id: prompt,
            method: "ask",
            questions: [{ id: "single", question: "Which option?", options: [{ label: "A" }, { label: "B" }] }],
          });
    } else {
      send({ type: "message_end", message: { role: "assistant", content: [{ type: "text", text: message }] } });
    }
  } else if (type === "extension_ui_response" && frame["id"] === prompt) {
    prompt = null;
    working = false;
    send({ type: "session_settled" });
  }
}
