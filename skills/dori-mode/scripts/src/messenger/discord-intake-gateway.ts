import type { Timers } from "./typing.ts";

/** One gateway connection; `close` takes a code because 1000/1001 end the session and 4000 keeps it resumable. */
export type DiscordSocket = {
  send(data: string): void;
  close(code: number): void;
  onMessage(cb: (data: string) => void): void;
  onClose(cb: (code: number) => void): void;
};

export type GatewayDeps = {
  readonly open: (url: string) => DiscordSocket;
  readonly timers: Timers;
  readonly sleep: (ms: number) => Promise<void>;
  readonly now: () => number;
  readonly warn: (line: string) => void;
};

export type GatewayHooks = {
  /** Every dispatch (op 0), READY and RESUMED included, in arrival order. */
  readonly onDispatch: (type: string, data: unknown) => void;
  /** A heartbeat was acknowledged: the connection is alive. */
  readonly onAck: () => void;
  /** The connection is down; `failures` counts closes in a row that never got (or kept) a live session. */
  readonly onDown: (failures: number) => Promise<void>;
};

export class DiscordFatalError extends Error {}

// GUILDS (thread/channel events), GUILD_MESSAGES, DIRECT_MESSAGES and the privileged MESSAGE_CONTENT.
export const DISCORD_INBOUND_INTENTS = (1 << 0) | (1 << 9) | (1 << 12) | (1 << 15);

export const DISCORD_GATEWAY = "wss://gateway.discord.gg/?v=10&encoding=json";

/** Close codes Discord documents as "do not reconnect"; retrying them only burns the daily identify budget. */
const FATAL: Readonly<Record<number, string>> = {
  4004: "authentication failed; check DORI_DISCORD_TOKEN",
  4010: "invalid shard",
  4011: "sharding required",
  4012: "invalid API version",
  4013: "invalid intents",
  4014: "disallowed intents; turn on Message Content Intent (Developer Portal > Bot > Privileged Gateway Intents)",
};
/** Closes after which the old session cannot be resumed: identify again. */
const FRESH = new Set([4003, 4005, 4007, 4009]);

const RESUMABLE_CLOSE = 4000;
const HELLO_TIMEOUT_MS = 30_000;
const BASE_DELAY_MS = 1_000;
const MAX_DELAY_MS = 120_000;
const STABLE_MS = 60_000;

type Frame = { readonly op: number; readonly s?: number | null; readonly t?: string | null; readonly d?: unknown };
type Session = { readonly id: string; readonly url: string };

/**
 * Hold a gateway session forever: identify, heartbeat (a missed ack closes the zombie connection), and on any close
 * resume at `resume_gateway_url` with the last sequence so Discord replays what was missed. Invalid-session and
 * non-resumable closes identify afresh; the documented fatal closes throw `DiscordFatalError`. Retries back off
 * from 1s doubling to 120s, reset once a session stayed live for a minute.
 */
export const runDiscordGateway = async (
  token: string,
  intents: number,
  deps: GatewayDeps,
  hooks: GatewayHooks,
): Promise<never> => {
  let session: Session | undefined;
  let seq: number | null = null;
  let failures = 0;
  for (;;) {
    const openedAt = deps.now();
    const { code, live } = await connect();
    const reason = FATAL[code];
    if (reason !== undefined) throw new DiscordFatalError(`discord gateway closed ${code}: ${reason}`);
    if (FRESH.has(code)) session = undefined;
    failures = live && deps.now() - openedAt >= STABLE_MS ? 1 : failures + 1;
    const delay = Math.min(MAX_DELAY_MS, BASE_DELAY_MS * 2 ** (failures - 1));
    deps.warn(`discord gateway closed code=${code}; ${session ? "resume" : "identify"} in ${delay / 1000}s`);
    await hooks.onDown(failures);
    await deps.sleep(delay);
  }

  function connect(): Promise<{ code: number; live: boolean }> {
    const done = Promise.withResolvers<{ code: number; live: boolean }>();
    let live = false;
    let acked = true;
    let beat: unknown;
    let dog: unknown;
    let ws: DiscordSocket;
    let ended = false;
    const finish = (code: number): void => {
      ended = true;
      if (beat !== undefined) deps.timers.clearInterval(beat);
      if (dog !== undefined) deps.timers.clearInterval(dog);
      beat = dog = undefined;
      done.resolve({ code, live });
    };
    const drop = (code: number): void => {
      try {
        ws.close(code);
      } catch {
        // already closing: the close event or finish below settles it
      }
      finish(code);
    };
    const send = (frame: Record<string, unknown>): void => ws.send(JSON.stringify(frame));
    try {
      ws = deps.open(session ? `${session.url}/?v=10&encoding=json` : DISCORD_GATEWAY);
    } catch (e) {
      deps.warn(`discord gateway open failed: ${e instanceof Error ? e.message : String(e)}`);
      return Promise.resolve({ code: 0, live: false });
    }
    dog = deps.timers.setInterval(() => drop(RESUMABLE_CLOSE), HELLO_TIMEOUT_MS);
    ws.onClose(finish);
    ws.onMessage((raw) => {
      if (ended) return; // a dropped connection's late frames must not move the session or sequence
      let m: Frame;
      try {
        m = JSON.parse(raw) as Frame;
      } catch {
        return;
      }
      if (typeof m.s === "number") seq = m.s;
      if (m.op === 10) {
        if (dog !== undefined) deps.timers.clearInterval(dog);
        dog = undefined;
        const interval = (m.d as { heartbeat_interval?: number } | undefined)?.heartbeat_interval ?? 41_250;
        beat = deps.timers.setInterval(() => {
          if (!acked) return drop(RESUMABLE_CLOSE); // zombie: no ack since the last beat
          acked = false;
          send({ op: 1, d: seq });
        }, interval);
        if (session) send({ op: 6, d: { token, session_id: session.id, seq } });
        else {
          seq = null;
          send({
            op: 2,
            d: {
              token,
              intents,
              properties: { os: process.platform, browser: "dori", device: "dori" },
              presence: { status: "online", since: null, activities: [], afk: false },
            },
          });
        }
      } else if (m.op === 11) {
        acked = true;
        hooks.onAck();
      } else if (m.op === 1) send({ op: 1, d: seq });
      else if (m.op === 7) drop(RESUMABLE_CLOSE);
      else if (m.op === 9) {
        if (m.d !== true) session = undefined;
        drop(RESUMABLE_CLOSE);
      } else if (m.op === 0 && typeof m.t === "string") {
        if (m.t === "READY") {
          const d = m.d as { session_id: string; resume_gateway_url: string };
          session = { id: d.session_id, url: d.resume_gateway_url };
          live = true;
        } else if (m.t === "RESUMED") live = true;
        hooks.onDispatch(m.t, m.d);
      }
    });
    return done.promise;
  }
};
