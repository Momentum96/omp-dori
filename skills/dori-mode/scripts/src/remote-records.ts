import { randomUUID } from "node:crypto";
import { mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { object, RemoteRejection } from "../../../../src/remote/client.ts";
import { takeLock } from "./checkpoint-lock.ts";

export const remoteId = /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,95}$/;

/** Durable receipts contain only session metadata and a request hash, not the initial prompt. */
export class RemoteRecords {
  private readonly dir: string;

  constructor(stateDir: string) {
    this.dir = join(stateDir, "remote");
  }

  path(kind: "session" | "open", id: string): string {
    if (!remoteId.test(id)) throw new Error("Use an exact native OMP session or request ID");
    return join(this.dir, `${kind}-${id}.json`);
  }

  read(path: string): Record<string, unknown> | undefined {
    try {
      return object(JSON.parse(readFileSync(path, "utf8")));
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined;
      throw error;
    }
  }

  store(path: string, value: unknown): void {
    mkdirSync(this.dir, { mode: 0o700, recursive: true });
    const pending = `${path}.${randomUUID()}`;
    try {
      writeFileSync(pending, `${JSON.stringify(value)}\n`, { flag: "wx", mode: 0o600 });
      renameSync(pending, path);
    } finally {
      rmSync(pending, { force: true });
    }
  }

  async exclusive<T>(key: string, work: () => Promise<T>): Promise<T> {
    mkdirSync(this.dir, { mode: 0o700, recursive: true });
    const lock = takeLock(join(this.dir, `${key}.lock`), Date.now(), (why) => new Error(why), {
      keepLiveOwner: true,
    });
    if (typeof lock !== "function") throw new Error(`Remote operation already in progress: ${key}`);
    try {
      return await work();
    } finally {
      lock();
    }
  }

  async close<T>(id: string, action: () => Promise<T>): Promise<T> {
    const path = this.path("session", id);
    return await this.exclusive(`session-${id}`, async () => {
      const previous = this.read(path) ?? {};
      // If the request lands but its reply is lost, wake must still be held.
      this.store(path, { ...previous, closePending: true });
      try {
        const result = await action();
        const status = object(result)?.["status"];
        if (status === "closed") this.store(path, { ...previous, closePending: false, closed: true });
        if (status === "refused") this.store(path, previous);
        return result;
      } catch (error) {
        if (error instanceof RemoteRejection) this.store(path, previous);
        throw error;
      }
    });
  }
}
