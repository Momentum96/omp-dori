import { expect, test } from "bun:test";
import {
  DISCORD_GATEWAY,
  DISCORD_INBOUND_INTENTS,
  DiscordFatalError,
  type GatewayHooks,
  runDiscordGateway,
} from "../src/messenger/discord-intake-gateway.ts";
import { flush, gatewayRig } from "./discord-fakes.ts";

const HB = 41_250;
const hello = { op: 10, d: { heartbeat_interval: HB } };
const ready = (s: number) => ({
  op: 0,
  s,
  t: "READY",
  d: { session_id: "S1", resume_gateway_url: "wss://resume.example" },
});

const start = () => {
  const rig = gatewayRig();
  const events: string[] = [];
  let downs: number[] = [];
  const hooks: GatewayHooks = {
    onDispatch: (t) => events.push(t),
    onAck: () => events.push("ACK"),
    onDown: async (n) => {
      downs.push(n);
    },
  };
  const run = runDiscordGateway("TOKEN", DISCORD_INBOUND_INTENTS, rig.deps, hooks);
  run.catch(() => {}); // asserted where a test ends it
  return {
    rig,
    run,
    events,
    downs: () => {
      const d = downs;
      downs = [];
      return d;
    },
  };
};

test("identifies with Message Content intents, then resumes at resume_gateway_url with the last sequence after a drop", async () => {
  const g = start();
  const rig = g.rig;
  const first = rig.last();
  expect(first.url).toBe(DISCORD_GATEWAY);
  first.push(hello);
  const identify = first.sent[0] as { op: number; d: { token: string; intents: number } };
  expect([identify.op, identify.d.token, identify.d.intents]).toEqual([2, "TOKEN", 32_769 + 512 + 4096]);
  first.push(ready(1));
  first.push({ op: 0, s: 2, t: "MESSAGE_CREATE", d: {} });
  first.drop(1006);
  await flush();
  expect(rig.intervals.size).toBe(1); // the old heartbeat stopped; only the new hello timeout is armed
  const second = rig.last();
  expect(second.url).toBe("wss://resume.example/?v=10&encoding=json");
  second.push(hello);
  expect(second.sent).toEqual([{ op: 6, d: { token: "TOKEN", session_id: "S1", seq: 2 } }]);
  second.push({ op: 0, s: 3, t: "RESUMED", d: {} });
  expect(g.events).toEqual(["READY", "MESSAGE_CREATE", "RESUMED"]);
  expect(rig.delays).toEqual([1_000]);
  second.drop(4004);
  await expect(g.run).rejects.toThrow("authentication failed");
});

test("a heartbeat without an ack since the last one closes the zombie connection resumably and reconnects", async () => {
  const g = start();
  const rig = g.rig;
  const ws = rig.last();
  ws.push(hello);
  ws.push(ready(5));
  rig.tick(HB);
  expect(ws.sent.at(-1)).toEqual({ op: 1, d: 5 });
  ws.push({ op: 11 });
  rig.tick(HB); // acked: beats again
  expect(ws.sent.filter((f) => f.op === 1)).toHaveLength(2);
  rig.tick(HB); // no ack for that one: zombie
  expect(ws.closedWith).toBe(4000);
  await flush();
  expect(rig.sockets).toHaveLength(2);
  expect(g.events).toContain("ACK");
  rig.last().drop(4014);
  const err = await g.run.catch((e: unknown) => e);
  expect(err).toBeInstanceOf(DiscordFatalError);
  expect(String(err)).toContain("Message Content Intent");
});

test("invalid session (not resumable) and non-resumable closes identify afresh; reconnect (op 7) resumes", async () => {
  const g = start();
  const rig = g.rig;
  rig.last().push(hello);
  rig.last().push(ready(1));
  rig.last().push({ op: 7 });
  await flush();
  rig.last().push(hello);
  expect(rig.last().sent[0]!.op).toBe(6);
  rig.last().push({ op: 9, d: false });
  await flush();
  expect(rig.last().url).toBe(DISCORD_GATEWAY);
  rig.last().push(hello);
  expect(rig.last().sent[0]!.op).toBe(2);
  rig.last().push(ready(1));
  rig.last().drop(4009); // session timed out: cannot resume
  await flush();
  rig.last().push(hello);
  expect(rig.last().sent[0]!.op).toBe(2);
  rig.last().drop(4013);
  await expect(g.run).rejects.toThrow("invalid intents");
});

test("failed connections back off 1s doubling to 120s; a session that stayed live a minute resets it", async () => {
  const g = start();
  const rig = g.rig;
  for (let i = 0; i < 9; i++) {
    rig.last().drop(1006); // never got a session
    await flush();
  }
  expect(rig.delays).toEqual([1, 2, 4, 8, 16, 32, 64, 120, 120].map((s) => s * 1000));
  expect(g.downs()).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9]);
  rig.last().push(hello);
  rig.last().push(ready(1));
  rig.now += 61_000;
  rig.last().drop(1001);
  await flush();
  expect(rig.delays.at(-1)).toBe(1_000);
  expect(g.downs()).toEqual([1]);
  // No hello within 30s: the connection is dropped and retried.
  rig.tick(30_000);
  expect(rig.sockets.at(-2)!.closedWith).toBeUndefined();
  expect(rig.last().closedWith).toBe(4000);
  await flush();
  rig.last().drop(4004);
  await expect(g.run).rejects.toBeInstanceOf(DiscordFatalError);
});
