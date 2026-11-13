import { expect, test } from "bun:test";
import { discordInbound } from "../src/messenger/discord-inbound.ts";
import { discordRestWorld, memoryDiscordCursor, ownerMessage } from "./discord-inbound-fakes.ts";
import { liveWorld } from "./discord-inbound-live-fakes.ts";

const cursorsFor = (initial: Record<string, string> = {}) => {
  const stores = new Map<string, ReturnType<typeof memoryDiscordCursor>>();
  return {
    stores,
    storeFor: (channel: string) => {
      const entry = stores.get(channel) ?? memoryDiscordCursor(initial[channel]);
      stores.set(channel, entry);
      return entry.store;
    },
  };
};

test("each configured channel and thread keeps its own cursor and order", async () => {
  const world = discordRestWorld([
    ownerMessage("100"),
    ownerMessage("101", { channel_id: "21" }),
    ownerMessage("102", { channel_id: "22" }),
  ]);
  const cursors = cursorsFor({ "22": "50" });
  const intake = await discordInbound(world.discord, cursors.storeFor, {
    ownerId: "10",
    channelIds: ["20", "21", "22"],
  });
  expect(intake.channels()).toEqual(["20", "21", "22"]);
  const received: string[] = [];
  const emit = (message: { channel: string; message: string }) =>
    received.push(`${message.channel}:${message.message}`);
  await intake.poll(emit);
  expect(received).toEqual(["22:102"]); // Channels 20 and 21 baselined cold; 22 resumed from its stored cursor.
  world.state.messages.push(
    ownerMessage("105", { channel_id: "21" }),
    ownerMessage("103"),
    ownerMessage("104", { channel_id: "21", author: { id: "11" } }),
  );
  await intake.poll(emit);
  expect(received).toEqual(["22:102", "20:103", "21:105"]);
  expect(cursors.storeFor("20").load()).toBe("103");
  expect(cursors.storeFor("21").load()).toBe("105");
  expect(cursors.storeFor("22").load()).toBe("102");
});

test("an explicit channel the bot cannot read is an error; the others are not read past it silently", async () => {
  const world = discordRestWorld([]);
  world.state.denied.add("21");
  await expect(
    discordInbound(world.discord, cursorsFor().storeFor, { ownerId: "10", channelIds: ["20", "21"] }),
  ).rejects.toThrow("access denied");
});

const guildWorld = () => {
  const world = discordRestWorld([ownerMessage("100"), ownerMessage("100", { channel_id: "30" })]);
  world.state.guilds = [{ id: "5", channels: ["20", "21", "22"], threads: ["30"] }];
  world.state.dm = "40";
  world.state.denied.add("22");
  return world;
};

test("scope all watches text channels, active threads and the owner DM, skipping channels it cannot read", async () => {
  const world = guildWorld();
  const intake = await discordInbound(world.discord, cursorsFor().storeFor, {
    ownerId: "10",
    channelIds: [],
    all: true,
  });
  expect(intake.channels().sort()).toEqual(["20", "21", "30", "40"]);
  const received: string[] = [];
  world.state.messages.push(ownerMessage("101", { channel_id: "30" }), ownerMessage("102", { channel_id: "40" }));
  await intake.poll((message) => received.push(`${message.channel}:${message.message}`));
  expect(received).toEqual(["30:101", "40:102"]);
  expect(world.requests.filter((request) => request.method === "POST").map((request) => request.body)).toEqual([
    '{"recipient_id":"10"}',
  ]);
});

test("scope all delivers a live message from a channel first seen at runtime, once, and persists its cursor", async () => {
  const world = await liveWorld([], { ownerId: "10", channelIds: [], all: true });
  const [socket] = world.sockets;
  if (!socket) throw new Error("gateway did not open");
  await world.connect(socket);
  expect(world.intake.channels()).toEqual([]);
  await world.dispatch(socket, ownerMessage("201", { channel_id: "50" }));
  await world.dispatch(socket, ownerMessage("201", { channel_id: "50" }));
  await world.dispatch(socket, ownerMessage("202", { channel_id: "50", author: { id: "11" } }));
  expect(world.received).toEqual(["50:201"]);
  expect(world.cursor("50")).toBe("202");
  expect(world.intake.channels()).toEqual(["50"]);
});
