import { lstatSync, readdirSync } from "node:fs";
import { createConnection } from "node:net";
import { join } from "node:path";
import { remoteSocketDir, remoteSocketPath } from "./socket-path.ts";

export type Session = {
  readonly session: string;
  readonly generation: string;
  readonly role: "dori" | "lane";
  readonly cwd: string;
  readonly status: string;
  readonly model?: string | null;
  readonly goal?: string;
  readonly pane?: string;
  readonly pid?: number;
  readonly sessionFile?: string;
};

export type Request = Readonly<{
  op: string;
  session?: string;
  generation?: string;
  id?: string;
  text?: string;
  steer?: boolean;
  force?: boolean;
  model?: string;
  cursor?: number;
  limit?: number;
  cwd?: string;
  name?: string;
  thinking?: string;
  worktree?: string;
  branch?: string;
  base?: string;
}>;

export const object = (value: unknown): Record<string, unknown> | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : undefined;

/** A definite rejection from a peer, unlike an error reading or transporting an acknowledgement. */
export class RemoteRejection extends Error {}

export function parseReply(text: string): unknown {
  const value = object(JSON.parse(text));
  if (value?.["ok"] === true) return value["data"];
  if (value?.["ok"] === false && typeof value["error"] === "string") throw new RemoteRejection(value["error"]);
  throw new Error("Invalid reply from Cataly remote");
}

function privateEntry(path: string, kind: "socket" | "directory"): boolean {
  try {
    const stat = lstatSync(path);
    return (
      process.getuid !== undefined &&
      stat.uid === process.getuid() &&
      (stat.mode & 0o077) === 0 &&
      (kind === "socket" ? stat.isSocket() : stat.isDirectory())
    );
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
    throw error;
  }
}

function query(path: string, request: Request): Promise<unknown> {
  if (!privateEntry(path, "socket")) throw new Error("No private live endpoint for this OMP session");
  return new Promise((resolve, reject) => {
    const socket = createConnection(path);
    let received = Buffer.alloc(0);
    socket.setTimeout(25_000, () => socket.destroy(new Error("Native OMP request timed out; do not retry blindly")));
    socket.once("error", reject);
    socket.once("end", () => reject(new Error("Native OMP endpoint ended without a receipt")));
    socket.once("connect", () => socket.write(`${JSON.stringify(request)}\n`));
    socket.on("data", (chunk: Buffer) => {
      if (received.length + chunk.length > 1024 * 1024) {
        socket.destroy(new Error("Native OMP reply exceeds the protocol limit"));
        return;
      }
      received = Buffer.concat([received, chunk]);
      const end = received.indexOf(10);
      if (end === -1) return;
      try {
        resolve(parseReply(received.subarray(0, end).toString()));
      } catch (error) {
        reject(error);
      }
      socket.destroy();
    });
  });
}

export function nativeRequest(stateDir: string, request: Request & { session: string }): Promise<unknown> {
  if (!privateEntry(remoteSocketDir(stateDir), "directory")) throw new Error("No private OMP endpoint directory");
  return query(remoteSocketPath(stateDir, request.session), request);
}

export async function liveSessions(stateDir: string): Promise<Session[]> {
  const dir = remoteSocketDir(stateDir);
  if (!privateEntry(dir, "directory")) return [];
  const endpoints = readdirSync(dir).filter((name) => /^[a-f0-9]{32}\.sock$/.test(name));
  const open: Session[] = [];
  for (let i = 0; i < endpoints.length; i += 16) {
    const batch = endpoints.slice(i, i + 16);
    await Promise.all(
      batch.map(async (name) => {
        const path = join(dir, name);
        try {
          const value = object(await query(path, { op: "info" }));
          if (
            typeof value?.["session"] === "string" &&
            typeof value["generation"] === "string" &&
            typeof value["cwd"] === "string" &&
            typeof value["status"] === "string" &&
            (value["role"] === "dori" || value["role"] === "lane") &&
            remoteSocketPath(stateDir, value["session"]) === path
          )
            open.push(value as Session);
        } catch (error) {
          // A shutdown between directory listing and connection is not a live session.
          if (!["ENOENT", "ECONNREFUSED", "ECONNRESET"].includes((error as NodeJS.ErrnoException).code ?? "")) {
            if (privateEntry(path, "socket")) throw error;
          }
        }
      }),
    );
  }
  return open.sort((a, b) => a.session.localeCompare(b.session));
}

export function selectSession(sessions: readonly Session[], id: string): Session {
  const exact = sessions.find((row) => row.session === id);
  if (exact !== undefined) return exact;
  const matches = sessions.filter((row) => row.session.startsWith(id));
  const only = matches.length === 1 ? matches[0] : undefined;
  if (only !== undefined) return only;
  throw new Error(matches.length > 1 ? "Ambiguous OMP session ID; use the full ID" : "No matching live OMP session");
}
