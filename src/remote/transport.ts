import { isAbsolute } from "node:path";
import { object, parseReply, type Request } from "./client.ts";

export const INPUT_LIMIT = 64 * 1024;
const targetPattern = /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$/;

export function peer(machine: string, executable: string): readonly string[] {
  if (!targetPattern.test(machine)) throw new Error("Invalid hc machine name");
  if (executable !== "dori" && !isAbsolute(executable)) throw new Error("--via must be an absolute dori path");
  return ["hc", "exec", "--on", machine, "--timeout", "55", "--stdin", "--", executable, "remote", "accept"];
}

export async function throughPeer(argv: readonly string[], request: Request): Promise<unknown> {
  const payload = `${JSON.stringify(request)}\n`;
  if (Buffer.byteLength(payload) > INPUT_LIMIT) throw new Error("Remote request is too large");
  const proc = Bun.spawn([...argv], { stdin: new Blob([payload]), stdout: "pipe", stderr: "pipe", timeout: 58_000 });
  const [stdout, code] = await Promise.all([
    new Response(proc.stdout).text(),
    proc.exited,
    // Drain, but don't print peer diagnostics (they may contain private paths or data).
    new Response(proc.stderr).arrayBuffer(),
  ]);
  if (code !== 0 || proc.signalCode)
    throw new Error(
      `Remote transport failed (${proc.signalCode ?? code}); mutation may have landed, do not retry blindly`,
    );
  return parseReply(stdout);
}

export async function readRequest(input: AsyncIterable<Buffer | string>): Promise<Request> {
  const chunks: Buffer[] = [];
  let length = 0;
  for await (const chunk of input) {
    const bytes = Buffer.from(chunk);
    length += bytes.length;
    if (length > INPUT_LIMIT) throw new Error("Remote request is too large");
    chunks.push(bytes);
  }
  const value = object(JSON.parse(Buffer.concat(chunks).toString()));
  if (!value || typeof value["op"] !== "string") throw new Error("Invalid remote request");
  for (const [key, item] of Object.entries(value)) {
    if (
      ([
        "op",
        "session",
        "generation",
        "id",
        "text",
        "model",
        "cwd",
        "name",
        "thinking",
        "worktree",
        "branch",
        "base",
      ].includes(key) &&
        typeof item === "string") ||
      (["steer", "force"].includes(key) && typeof item === "boolean") ||
      (["cursor", "limit"].includes(key) && typeof item === "number" && Number.isSafeInteger(item) && item >= 0)
    )
      continue;
    throw new Error(`Invalid remote field ${key}`);
  }
  return value as Request;
}
