import { chmodSync, lstatSync, mkdirSync, unlinkSync } from "node:fs";
import { createConnection, createServer, type Socket } from "node:net";
import { dirname } from "node:path";
import { takeLock } from "../skills/dori-mode/scripts/src/checkpoint-lock.ts";
import { validateRemoteSocket, validateRemoteSocketDir } from "../src/remote/socket-path.ts";

const REQUEST_BYTES = 64 * 1024;
const RESPONSE_BYTES = 512 * 1024;
const CONNECTIONS = 32;

function socketExists(path: string): boolean {
  try {
    lstatSync(path);
    return true;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    return false;
  }
}

function probe(path: string): Promise<"live" | "stale"> {
  return new Promise((resolve, reject) => {
    const client = createConnection(path);
    client.setTimeout(1000, () => client.destroy(new Error("Native remote socket ownership check timed out")));
    client.on("error", (error: NodeJS.ErrnoException) => {
      if (error.code === "ECONNREFUSED" || error.code === "ENOENT") resolve("stale");
      else reject(error);
    });
    client.on("connect", () => {
      client.destroy();
      resolve("live");
    });
  });
}

/** A listener owns only its verified Unix pathname. Connections carry exactly one bounded JSON line. */
export class NativeRemoteSocket {
  private readonly clients = new Set<Socket>();
  private readonly server;
  private stopped = false;
  private ready = false;

  constructor(
    readonly path: string,
    private readonly handle: (request: unknown) => Promise<unknown>,
    private readonly onError: (error: unknown) => void,
  ) {
    this.server = createServer({ allowHalfOpen: true }, (socket) => this.connect(socket));
    this.server.on("error", (error) => {
      if (this.ready && !this.stopped) {
        this.stop();
        this.onError(error);
      }
    });
  }

  async start(): Promise<void> {
    const dir = dirname(this.path);
    try {
      mkdirSync(dir, { mode: 0o700 });
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
    }
    validateRemoteSocketDir(dir);
    const lock = `${this.path}.lock`;
    if (socketExists(lock)) {
      const stat = lstatSync(lock);
      if (!stat.isFile() || stat.uid !== process.getuid?.() || (stat.mode & 0o7777) !== 0o600)
        throw new Error("Unsafe native remote socket lock");
    }
    const release = takeLock(lock, Date.now(), (detail) => new Error(`Native remote socket lock ${detail}`), {
      keepLiveOwner: true,
    });
    if (typeof release !== "function") throw new Error(`Native remote endpoint is already starting (${release})`);
    try {
      if (socketExists(this.path)) {
        const before = validateRemoteSocket(this.path);
        if ((await probe(this.path)) === "live") throw new Error("Native remote endpoint already has a live owner");
        if (this.stopped) return;
        validateRemoteSocketDir(dir);
        if (socketExists(this.path)) {
          const after = validateRemoteSocket(this.path);
          if (before.ino !== after.ino || before.dev !== after.dev)
            throw new Error("Native remote socket changed during ownership check");
          unlinkSync(this.path);
        }
      }
      if (this.stopped) return;
      await new Promise<void>((resolve, reject) => {
        this.server.once("error", reject);
        this.server.listen(this.path, () => {
          this.server.removeListener("error", reject);
          try {
            if (this.stopped) {
              this.server.close();
              resolve();
              return;
            }
            chmodSync(this.path, 0o600);
            validateRemoteSocket(this.path);
            this.server.unref();
            this.ready = true;
            resolve();
          } catch (error) {
            this.server.close();
            reject(error);
          }
        });
      });
    } finally {
      release();
    }
  }

  disconnect(): void {
    for (const socket of this.clients) socket.destroy();
    this.clients.clear();
  }

  stop(): void {
    this.stopped = true;
    this.disconnect();
    if (this.server.listening) this.server.close();
  }

  private connect(socket: Socket): void {
    if (this.stopped || !this.ready || this.clients.size >= CONNECTIONS) {
      socket.destroy();
      return;
    }
    this.clients.add(socket);
    const timer = setTimeout(() => socket.destroy(), 10_000);
    timer.unref();
    socket.on("error", () => socket.destroy());
    socket.on("close", () => {
      clearTimeout(timer);
      this.clients.delete(socket);
    });
    let size = 0;
    let handled = false;
    const chunks: Buffer[] = [];
    const reply = (value: unknown) => {
      if (socket.destroyed) return;
      const json = JSON.stringify(value);
      socket.end(
        Buffer.byteLength(json) + 1 <= RESPONSE_BYTES
          ? `${json}\n`
          : '{"ok":false,"error":"Native remote response exceeds its byte limit"}\n',
      );
    };
    const fail = (error: string) => reply({ ok: false, error });
    socket.on("end", () => {
      if (!handled) fail("Expected one native remote JSON line");
    });
    socket.on("data", (chunk: Buffer) => {
      if (handled) return;
      size += chunk.length;
      const end = chunk.indexOf(10);
      if (size > REQUEST_BYTES || (end >= 0 && end !== chunk.length - 1)) {
        handled = true;
        fail("Expected one native remote JSON line within its byte limit");
        return;
      }
      if (end < 0) {
        chunks.push(chunk);
        return;
      }
      handled = true;
      chunks.push(chunk.subarray(0, end));
      let request: unknown;
      try {
        request = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(Buffer.concat(chunks)));
      } catch {
        fail("Invalid native remote JSON");
        return;
      }
      void this.handle(request).then(reply, () => fail("Native remote could not handle the request"));
    });
  }
}
