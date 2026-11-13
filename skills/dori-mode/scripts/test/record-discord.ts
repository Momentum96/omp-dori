import { appendFileSync, readFileSync } from "node:fs";

// Preloaded into a CLI process by discord-cli.test.ts; nothing leaves the machine.
// fetch: each request is appended to $DORI_TEST_FETCH_LOG and answered from the JSON route table in
// $DORI_TEST_DISCORD_ROUTES ({"METHOD /path": body}); an unknown route is a 404.
// WebSocket: plays the frames in $DORI_TEST_GATEWAY ({"frames": [...], "close": code}) and records what the client
// sent as {"ws": frame} lines in the same log.
const log = process.env["DORI_TEST_FETCH_LOG"] ?? "";
const routes = JSON.parse(readFileSync(process.env["DORI_TEST_DISCORD_ROUTES"] ?? "", "utf8")) as Record<
  string,
  unknown
>;

globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
  const url = new URL(String(input));
  const key = `${init?.method ?? "GET"} ${url.pathname.replace("/api/v10", "")}${url.search}`;
  appendFileSync(
    log,
    `${JSON.stringify({ key, body: typeof init?.body === "string" ? JSON.parse(init.body) : null })}\n`,
  );
  if (!(key in routes)) return new Response(JSON.stringify({ message: "Unknown" }), { status: 404 });
  const body = routes[key];
  return body === null ? new Response(null, { status: 204 }) : new Response(JSON.stringify(body), { status: 200 });
}) as typeof fetch;

type Listener = (event: { data?: string; code?: number }) => void;

class Gateway {
  private readonly listeners = new Map<string, Listener[]>();
  constructor(readonly url: string) {
    const script = JSON.parse(readFileSync(process.env["DORI_TEST_GATEWAY"] ?? "", "utf8")) as {
      frames: unknown[];
      close: number;
    };
    setTimeout(() => {
      for (const frame of script.frames)
        for (const l of this.listeners.get("message") ?? []) l({ data: JSON.stringify(frame) });
      setTimeout(() => {
        for (const l of this.listeners.get("close") ?? []) l({ code: script.close });
      }, 50);
    }, 0);
  }
  addEventListener(type: string, listener: Listener): void {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }
  send(data: string): void {
    appendFileSync(log, `${JSON.stringify({ ws: JSON.parse(data) })}\n`);
  }
  close(): void {}
}

Object.defineProperty(globalThis, "WebSocket", { value: Gateway });
