import { expect, test } from "bun:test";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { discordInbound, formatDiscordInbound } from "../src/messenger/discord-inbound.ts";
import { fileDiscordInboundStore } from "../src/messenger/discord-inbound-state.ts";
import { routeConfigFromTopology, snowflakeAt } from "../src/messenger/discord-route.ts";
import { discordRestWorld, memoryDiscordCursor, ownerMessage } from "./discord-inbound-fakes.ts";
import { liveWorld, settle } from "./discord-inbound-live-fakes.ts";
import { withState } from "./fakes.ts";

const config = routeConfigFromTopology({
  ownerId: "10",
  topicChannels: { work: "20" },
  testChannel: null,
  bots: { omo: { userId: "98", roleId: null }, omp: { userId: "99", roleId: null } },
});
const BASE = 1_763_000_000_000;
const id = (offset: number) => snowflakeAt(BASE + offset);
const plain = (offset: number) => ownerMessage(id(offset), { content: "그냥" });
const attachmentOnly = (offset: number) =>
  ownerMessage(id(offset), {
    content: "",
    attachments: [{ filename: "a.png", size: 1, url: "https://cdn.test/a.png" }],
  });
const toOmp = (offset: number) => ownerMessage(id(offset), { content: "<@99> 해줘", mentions: [{ id: "99" }] });
const toOmo = (offset: number) => ownerMessage(id(offset), { content: "<@98> 해줘", mentions: [{ id: "98" }] });

/** A gateway intake for the omp bot whose routing clock is the fake timer clock, offset to BASE. */
const routedLive = async () => {
  let clock = () => 0;
  const routing = { bot: "omp" as const, config, now: () => BASE + clock() };
  const world = await liveWorld([], { ownerId: "10", channelIds: ["20"], routing }, { "20": "100" });
  clock = world.now;
  const [socket] = world.sockets;
  if (!socket) throw new Error("gateway did not open");
  await world.connect(socket);
  const at = async (offset: number) => {
    world.advance(offset - world.now());
    await settle();
  };
  return { world, socket, at };
};

test("a message waiting for its burst decision follows the next addressed message, in order", async () => {
  const { world, socket, at } = await routedLive();
  await at(1_000);
  await world.dispatch(socket, attachmentOnly(1_000));
  expect(world.received).toEqual([]);
  expect(world.cursor("20")).toBe("100"); // Held: the cursor must not pass an undecided message.
  await at(5_000);
  await world.dispatch(socket, toOmp(5_000));
  expect(world.received).toEqual([`20:${id(1_000)}`, `20:${id(5_000)}`]);
  expect(world.cursor("20")).toBe(id(5_000));
  expect(world.intake.wakeIn()).toBeUndefined();
});

test("an unaddressed message is dropped by the burst timer with no output, and the cursor still moves", async () => {
  const { world, socket, at } = await routedLive();
  await at(1_000);
  await world.dispatch(socket, plain(1_000));
  await at(10_999);
  expect(world.cursor("20")).toBe("100");
  await at(11_000);
  expect(world.cursor("20")).toBe(id(1_000));
  await world.dispatch(socket, toOmo(12_000));
  await at(23_000);
  expect(world.received).toEqual([]);
  expect(world.lines).toEqual([]);
  expect(world.cursor("20")).toBe(id(12_000));
});

test("a gateway edit that newly mentions omp becomes one request; repeats and other edits do not", async () => {
  const { world, socket, at } = await routedLive();
  await at(1_000);
  await world.dispatch(socket, plain(1_000));
  await world.dispatch(socket, toOmo(2_000));
  await at(13_000);
  const editedAt = new Date(BASE + 20_000).toISOString();
  const addressed = { ...toOmp(1_000), id: id(1_000), edited_timestamp: editedAt };
  const omoEdit = { ...toOmo(2_000), content: "<@98> 다시", edited_timestamp: editedAt };
  world.rest.state.messages = [addressed, omoEdit];
  const update = (message: typeof addressed, s: number) => {
    const { id: messageId, channel_id, content, edited_timestamp } = message;
    socket.receive({ op: 0, t: "MESSAGE_UPDATE", s, d: { id: messageId, channel_id, content, edited_timestamp } });
  };
  update(addressed, 100);
  await settle();
  update(addressed, 101);
  update(omoEdit, 102);
  await settle();
  expect(world.received).toEqual([`20:${id(1_000)}`]);
  expect(world.lines.join("\n").split("\n")).toEqual([
    `INBOUND discord 20 ${id(1_000)} from=owner ref=- "<@99> 해줘"`,
    `INBOUND_EDIT discord 20 ${id(1_000)} {"id":"e:omp:${id(1_000)}:${BASE + 20_000}","editedAt":"${editedAt}"}`,
  ]);
});

test("a restart while a burst decision is open delivers each message exactly once", async () => {
  const history = [attachmentOnly(1_000), toOmp(4_000)];
  const cursor = memoryDiscordCursor("100");
  const lines: string[] = [];
  const emit = (message: Parameters<typeof formatDiscordInbound>[0]) => lines.push(formatDiscordInbound(message));
  const start = async (offset: number) =>
    discordInbound(discordRestWorld(history).discord, () => cursor.store, {
      ownerId: "10",
      channelIds: ["20"],
      routing: { bot: "omp", config, now: () => BASE + offset },
    });
  const first = await start(1_001);
  await first.accept(history[0] ?? attachmentOnly(0), emit);
  expect(cursor.store.load()).toBe("100"); // The process dies here, before the decision.
  const second = await start(5_000);
  await second.poll(emit);
  await second.poll(emit);
  expect(lines.map((line) => line.split(" ")[3])).toEqual([id(1_000), id(4_000)]);
  expect(cursor.store.load()).toBe(id(4_000));
});

test("routing state persists beside the cursor; a pre-routing cursor file still loads; a bad one stops intake", async () => {
  const state = withState();
  try {
    const path = join(state.dir, "discord-inbound", "20.json");
    fileDiscordInboundStore(state.dir, "20").save("100"); // A cursor written before routing existed.
    const run = async (history: ReturnType<typeof ownerMessage>[]) => {
      const lines: string[] = [];
      const intake = await discordInbound(
        discordRestWorld(history).discord,
        (channel) => fileDiscordInboundStore(state.dir, channel),
        { ownerId: "10", channelIds: ["20"], routing: { bot: "omp", config, now: () => BASE + 60_000 } },
      );
      await intake.poll((message) => lines.push(formatDiscordInbound(message)));
      return lines;
    };
    expect(await run([toOmp(1_000)])).toHaveLength(1);
    const saved = JSON.parse(readFileSync(path, "utf8"));
    expect(saved.route).toEqual({
      assignee: null,
      recent: [{ id: id(1_000), addressed: ["omp"], reason: "explicit" }],
    });
    writeFileSync(path, JSON.stringify({ ...saved, route: { assignee: "nobody", recent: [] } }));
    await expect(run([toOmp(1_000), toOmp(2_000)])).rejects.toThrow("routing state is invalid");
  } finally {
    state.done();
  }
});

test("a topology that names another omp bot or owner is refused at startup", async () => {
  const other = routeConfigFromTopology({
    ownerId: "10",
    topicChannels: { work: "20" },
    bots: { omo: { userId: "98", roleId: null }, omp: { userId: "97", roleId: null } },
  });
  const start = (routing: typeof config) =>
    discordInbound(discordRestWorld([]).discord, () => memoryDiscordCursor("100").store, {
      ownerId: "10",
      channelIds: ["20"],
      routing: { bot: "omp", config: routing, now: () => BASE },
    });
  await expect(start(other)).rejects.toThrow("topology names another omp bot or owner");
  await expect(start({ ...config, ownerId: "11" })).rejects.toThrow("topology names another omp bot or owner");
});
