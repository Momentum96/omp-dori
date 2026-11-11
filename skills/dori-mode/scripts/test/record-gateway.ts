// Preloaded by messenger-reconnect.test.ts. No socket is opened and elapsed time is synthetic.
const active = new Set<number>();
const delays: number[] = [];
let opened = 0;
let cleared = 0;
let identified = 0;
let now = 0;

type GatewayEvent = { readonly data?: string; readonly code?: number };

class Gateway {
  readonly listeners = new Map<string, Array<(event: GatewayEvent) => void>>();

  constructor(_url: string) {
    if (active.size !== 0) throw new Error("a previous gateway heartbeat is still active");
    opened++;
    queueMicrotask(() => {
      for (const listener of this.listeners.get("message") ?? []) {
        listener({ data: JSON.stringify({ op: 10, d: { heartbeat_interval: 41_250 } }) });
      }
      now += opened === 8 ? 61_001 : 100;
      for (const listener of this.listeners.get("close") ?? []) listener({ code: 1006 });
    });
  }

  addEventListener(type: string, listener: (event: GatewayEvent) => void): void {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  send(data: string): void {
    const frame = JSON.parse(data) as { op: number; d?: { presence?: { status?: string } } };
    if (frame.op === 2 && frame.d?.presence?.status === "online") identified++;
  }

  close(): void {
    if (active.size !== 0) throw new Error("gateway close did not clear its heartbeat");
  }
}

Object.defineProperty(globalThis, "WebSocket", { value: Gateway });
Object.defineProperty(globalThis, "fetch", {
  value: () => {
    throw new Error("network access is disabled in the gateway fixture");
  },
});
Object.defineProperty(globalThis, "setInterval", {
  value: () => {
    active.add(opened);
    return opened;
  },
});
Object.defineProperty(globalThis, "clearInterval", {
  value: (handle: number) => {
    if (active.delete(handle)) cleared++;
  },
});
Date.now = () => now;
Object.defineProperty(Bun, "sleep", {
  value: async (ms: number) => {
    delays.push(ms);
    if (delays.length === 8) {
      console.log(`GATEWAY_QA ${JSON.stringify({ opened, cleared, identified, delays, active: active.size })}`);
      process.exit(0);
    }
  },
});
