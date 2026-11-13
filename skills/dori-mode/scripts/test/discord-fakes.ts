import type { DiscordSocket, GatewayDeps } from "../src/messenger/discord-intake-gateway.ts";
import type { HttpRequest, HttpResponse } from "../src/messenger/http.ts";

/** One fake gateway connection: frames the client sent, and hooks to play the server's side. */
export class FakeSocket implements DiscordSocket {
  readonly sent: { op: number; d?: unknown }[] = [];
  closedWith: number | undefined;
  private onMsg: (d: string) => void = () => {};
  private onEnd: (code: number) => void = () => {};
  constructor(readonly url: string) {}
  send(data: string): void {
    this.sent.push(JSON.parse(data) as { op: number; d?: unknown });
  }
  close(code: number): void {
    this.closedWith = code;
    queueMicrotask(() => this.onEnd(code));
  }
  onMessage(cb: (d: string) => void): void {
    this.onMsg = cb;
  }
  onClose(cb: (code: number) => void): void {
    this.onEnd = cb;
  }
  push(frame: Record<string, unknown>): void {
    this.onMsg(JSON.stringify(frame));
  }
  /** The server drops the connection. */
  drop(code: number): void {
    this.onEnd(code);
  }
}

/** Fake sockets, interval timers that the test fires by hand, recorded backoff sleeps and a settable clock. */
export const gatewayRig = () => {
  const sockets: FakeSocket[] = [];
  const intervals = new Map<number, { readonly cb: () => void; readonly ms: number }>();
  const delays: number[] = [];
  let handle = 0;
  const rig = {
    sockets,
    intervals,
    delays,
    now: 0,
    last: (): FakeSocket => sockets.at(-1)!,
    /** Fire every live interval of the given period once. */
    tick(ms: number): void {
      for (const t of [...intervals.values()]) if (t.ms === ms) t.cb();
    },
    deps: undefined as unknown as GatewayDeps,
  };
  rig.deps = {
    open: (url) => {
      const s = new FakeSocket(url);
      sockets.push(s);
      return s;
    },
    timers: {
      setInterval: (cb, ms) => {
        intervals.set(++handle, { cb, ms });
        return handle;
      },
      clearInterval: (h) => {
        intervals.delete(h as number);
      },
    },
    sleep: async (ms) => {
      delays.push(ms);
    },
    now: () => rig.now,
    warn: () => {},
  };
  return rig;
};

/** Let queued microtasks and promise chains run. */
export const flush = async (): Promise<void> => {
  for (let i = 0; i < 20; i++) await Promise.resolve();
};

/** A Discord REST fake answering by `METHOD path` (query string included); unknown routes are a 404. */
export const restFake = (routes: Record<string, unknown | ((req: HttpRequest) => unknown)>) => {
  const seen: string[] = [];
  const http = async (req: HttpRequest): Promise<HttpResponse> => {
    const key = `${req.method} ${req.url.replace("https://discord.com/api/v10", "")}`;
    seen.push(key);
    const route = routes[key];
    if (route === undefined) return { status: 404, headers: {}, body: JSON.stringify({ message: "Unknown" }) };
    const body = typeof route === "function" ? (route as (r: HttpRequest) => unknown)(req) : route;
    return { status: 200, headers: {}, body: JSON.stringify(body) };
  };
  return { http, seen };
};
