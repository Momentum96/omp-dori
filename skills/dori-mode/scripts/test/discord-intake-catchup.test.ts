import { afterEach, beforeEach, expect, test } from "bun:test";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { Discord } from "../src/messenger/discord.ts";
import { type DcStore, fileDcStore, snowflakeAt } from "../src/messenger/discord-intake-cursor.ts";
import { DiscordFatalError, type GatewayHooks } from "../src/messenger/discord-intake-gateway.ts";
import { type DcInbound, DiscordIntake } from "../src/messenger/discord-intake.ts";
import { discordInboundLoop } from "../src/messenger/discord-intake-loop.ts";
import { restFake } from "./discord-fakes.ts";
import { fakeClock, withState } from "./fakes.ts";

const OWNER = "1197460639967359091";
const GUILD = "1570779238913671208";
const TEXT = "1570779429763027068";
const DM = "1570700000000000009";
const T0 = Date.parse("2026-11-13T12:00:00Z");
const at = (ms: number, k = 0) => (BigInt(snowflakeAt(ms)) + BigInt(k)).toString();

const state = withState();
beforeEach(() => mkdirSync(state.dir, { recursive: true }));
afterEach(() => state.done());

/** REST answers for the visible channels; `history` is what Discord lists after the cursor, newest first. */
const routes = (after: string, history: readonly { id: string; author: string; text: string }[]) => ({
  "POST /users/@me/channels": { id: DM, type: 1, last_message_id: null },
  [`GET /guilds/${GUILD}/channels`]: [
    { id: TEXT, type: 0, last_message_id: history[0]?.id ?? after },
    { id: "1570779431675494451", type: 15 },
  ],
  [`GET /guilds/${GUILD}/threads/active`]: { threads: [] },
  [`GET /channels/${TEXT}/messages?after=${after}&limit=100`]: history.map((m) => ({
    id: m.id,
    channel_id: TEXT,
    type: 0,
    author: { id: m.author },
    content: m.text,
  })),
});

const make = (
  store: DcStore,
  r: Record<string, unknown>,
  now: () => number = () => T0 + 3_600_000,
  emit?: (i: DcInbound) => void,
) => {
  const rest = restFake(r);
  const out: DcInbound[] = [];
  const inbound = new DiscordIntake(
    new Discord(rest.http, fakeClock(0), "B"),
    store,
    { ownerId: OWNER, guildId: GUILD },
    emit ?? ((i) => out.push(i)),
    () => {},
    now,
  );
  return { inbound, out, rest };
};

test("the first run records a starting point and replays no history", async () => {
  const path = join(state.dir, "discord-cursor.json");
  const { inbound, out, rest } = make(
    fileDcStore(path, () => {}),
    {},
  );
  expect(await inbound.catchUp()).toBe(true);
  expect([out, rest.seen]).toEqual([[], []]);
  expect(JSON.parse(readFileSync(path, "utf8")).upTo).toBe(snowflakeAt(T0 + 3_600_000 - 60_000));
});

test("a restart catches up what arrived while it was down, in order, owner only, and not again", async () => {
  const path = join(state.dir, "discord-cursor.json");
  const store = fileDcStore(path, () => {});
  const seen = at(T0 - 60_000, 7);
  store.save({ upTo: at(T0 - 120_000), channels: { [TEXT]: seen } });
  const history = [
    { id: at(T0 + 3, 1), author: OWNER, text: "third" },
    { id: at(T0 + 2, 1), author: "42", text: "stranger" },
    { id: at(T0 + 1, 1), author: OWNER, text: "first" },
  ];
  const { inbound, out, rest } = make(store, routes(seen, history));
  expect(await inbound.catchUp()).toBe(true);
  expect(out.map((i) => [i.source, i.channel, i.text])).toEqual([
    ["channel", TEXT, "first"],
    ["channel", TEXT, "third"],
  ]);
  expect(rest.seen).not.toContain(`GET /channels/${DM}/messages?after=${seen}&limit=100`); // no new DM: skipped
  const cursor = JSON.parse(readFileSync(path, "utf8"));
  expect(cursor.channels[TEXT]).toBe(history[0]!.id);
  expect(cursor.upTo).toBe(snowflakeAt(T0 + 3_600_000 - 60_000));
  // The same history again (a later catch-up overlapping it) prints nothing.
  const again = make(store, routes(history[0]!.id, []));
  await again.inbound.catchUp();
  expect(again.out).toEqual([]);
});

test("a line that failed to print is delivered again after a restart (at-least-once)", async () => {
  const store = fileDcStore(join(state.dir, "c.json"), () => {});
  const seen = at(T0, 1);
  store.save({ upTo: seen, channels: {} });
  const history = [
    { id: at(T0 + 2, 1), author: OWNER, text: "second" },
    { id: at(T0 + 1, 1), author: OWNER, text: "first" },
  ];
  let printed = 0;
  const failing = make(
    store,
    routes(seen, history),
    () => T0 + 60_000,
    () => {
      if (++printed === 2) throw new Error("EPIPE");
    },
  );
  await expect(failing.inbound.catchUp()).rejects.toThrow("EPIPE");
  const retry = make(store, routes(history[1]!.id, [history[0]!]));
  await retry.inbound.catchUp();
  expect(retry.out.map((i) => i.text)).toEqual(["second"]);
});

test("an unreadable channel keeps the watermark where it was, so the gap is read again later", async () => {
  const store = fileDcStore(join(state.dir, "c.json"), () => {});
  const upTo = at(T0);
  store.save({ upTo, channels: {} });
  const r = routes(upTo, [{ id: at(T0 + 1, 1), author: OWNER, text: "x" }]);
  delete (r as Record<string, unknown>)[`GET /channels/${TEXT}/messages?after=${upTo}&limit=100`];
  const { inbound } = make(store, r);
  expect(await inbound.catchUp()).toBe(false);
  expect(store.load().upTo).toBe(upTo);
});

test("a corrupt cursor warns and starts fresh instead of replaying history", () => {
  const path = join(state.dir, "bad.json");
  writeFileSync(path, "{not json");
  const warns: string[] = [];
  expect(fileDcStore(path, (w) => warns.push(w)).load()).toEqual({ channels: {} });
  expect(warns[0]).toContain("unparseable");
});

test("--loop: one INBOUND_READY, READY catches up, resume replays are skipped, acks move the watermark", async () => {
  const store = fileDcStore(join(state.dir, "c.json"), () => {});
  const seen = at(T0, 1);
  store.save({ upTo: seen, channels: { [TEXT]: seen } });
  const missed = { id: at(T0 + 5, 1), author: OWNER, text: "while down" };
  let now = T0 + 600_000;
  const { inbound, out } = make(store, routes(seen, [missed]), () => now);
  const readies: string[] = [];
  const sleeps: number[] = [];
  const warns: string[] = [];
  let hooks: GatewayHooks | undefined;
  const gate = Promise.withResolvers<never>();
  const loop = discordInboundLoop(inbound, 2, {
    gateway: (h) => {
      hooks = h;
      return gate.promise;
    },
    sleep: async (ms) => {
      sleeps.push(ms);
      throw new Error("STOP");
    },
    now: () => now,
    ready: (mode) => readies.push(mode),
    warn: (w) => warns.push(w),
  });
  hooks!.onDispatch("READY", { session_id: "S", resume_gateway_url: "wss://r" });
  const live = { id: at(T0 + 600_000, 1), channel_id: TEXT, guild_id: GUILD, type: 0, author: { id: OWNER } };
  hooks!.onDispatch("MESSAGE_CREATE", { ...live, content: "live" });
  hooks!.onDispatch("RESUMED", {});
  hooks!.onDispatch("MESSAGE_CREATE", { ...live, content: "live" }); // replayed by the resume
  now += 30_000;
  hooks!.onAck();
  gate.reject(new DiscordFatalError("discord gateway closed 4014: disallowed intents"));
  await expect(loop).rejects.toThrow("STOP");
  expect(out.map((i) => i.text)).toEqual(["while down", "live"]);
  expect(readies).toEqual(["gateway"]);
  expect(store.load().upTo).toBe(snowflakeAt(now - 60_000));
  expect(warns[0]).toContain("reading channels over REST every 2 min");
  expect(sleeps).toEqual([120_000]);
});

test("one-shot (no --loop) reads the channels once over REST and returns", async () => {
  const store = fileDcStore(join(state.dir, "c.json"), () => {});
  const seen = at(T0, 1);
  store.save({ upTo: seen, channels: {} });
  const { inbound, out } = make(store, routes(seen, [{ id: at(T0 + 1, 1), author: OWNER, text: "hi" }]));
  const readies: string[] = [];
  await discordInboundLoop(inbound, 0, {
    gateway: () => Promise.reject(new Error("no gateway in one-shot")),
    sleep: async () => {},
    now: () => T0,
    ready: (m) => readies.push(m),
    warn: () => {},
  });
  expect([readies, out.map((i) => i.text)]).toEqual([["rest"], ["hi"]]);
});
