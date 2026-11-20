import { randomUUID } from "node:crypto";
import {
  constants,
  closeSync,
  fstatSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { object } from "./client.ts";

type Policy = { readonly control: boolean; readonly wake: boolean };
const disabled: Policy = { control: false, wake: false };
const pathOf = (stateDir: string): string => join(stateDir, "remote-policy.json");

export function remotePolicy(stateDir: string): Policy {
  let fd: number;
  try {
    fd = openSync(pathOf(stateDir), constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return disabled;
    throw error;
  }
  try {
    const stat = fstatSync(fd);
    if (!stat.isFile() || stat.uid !== process.getuid?.() || (stat.mode & 0o077) !== 0 || stat.size > 4096)
      throw new Error("Remote policy must be an owner-only regular file");
    const value = object(JSON.parse(readFileSync(fd, "utf8")));
    if (typeof value?.["control"] !== "boolean" || typeof value["wake"] !== "boolean")
      throw new Error("Invalid remote policy; refusing remote access");
    return { control: value["control"], wake: value["wake"] };
  } finally {
    closeSync(fd);
  }
}

export function setRemotePolicy(stateDir: string, key: keyof Policy, enabled: boolean): Policy {
  const updated = { ...remotePolicy(stateDir), [key]: enabled };
  const path = pathOf(stateDir);
  const temporary = `${path}.${randomUUID()}`;
  mkdirSync(stateDir, { recursive: true, mode: 0o700 });
  try {
    writeFileSync(temporary, `${JSON.stringify(updated)}\n`, { mode: 0o600, flag: "wx" });
    renameSync(temporary, path);
  } finally {
    rmSync(temporary, { force: true });
  }
  return updated;
}
