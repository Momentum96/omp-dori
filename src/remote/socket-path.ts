import { createHash } from "node:crypto";
import { lstatSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const hash = (value: string) => createHash("sha256").update(value).digest("hex");

function uid(): number {
  if (typeof process.getuid !== "function") throw new Error("Native remote requires Unix user ownership");
  return process.getuid();
}

export function remoteSocketDir(stateDir: string): string {
  const name = `cataly-remote-${uid()}-${hash(resolve(stateDir)).slice(0, 20)}`;
  const preferred = join(tmpdir(), name);
  // Darwin's sockaddr_un holds 103 pathname bytes plus NUL. Its per-user TMPDIR is commonly too long.
  const max = process.platform === "darwin" ? 103 : 107;
  const dir = Buffer.byteLength(preferred) + 38 <= max ? preferred : join("/tmp", name);
  if (Buffer.byteLength(dir) + 38 > max) throw new Error("Native remote socket directory path is too long");
  return dir;
}

export function remoteSocketPath(stateDir: string, session: string): string {
  return join(remoteSocketDir(stateDir), `${hash(session).slice(0, 32)}.sock`);
}

export function validateRemoteSocketDir(dir: string): void {
  const stat = lstatSync(dir);
  if (!stat.isDirectory() || stat.uid !== uid() || (stat.mode & 0o7777) !== 0o700)
    throw new Error("Unsafe native remote socket directory");
}

export function validateRemoteSocket(path: string) {
  const stat = lstatSync(path);
  if (!stat.isSocket() || stat.uid !== uid() || (stat.mode & 0o7777) !== 0o600)
    throw new Error("Unsafe native remote socket");
  return stat;
}
