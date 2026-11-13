import { expect, test } from "bun:test";
import { ownerMessage } from "./discord-inbound-fakes.ts";
import { liveWorld, settle } from "./discord-inbound-live-fakes.ts";

const options = { ownerId: "10", channelIds: ["20"] };

test("a live owner message is emitted at once, with the minimum intents and no presence override", async () => {
  const world = await liveWorld([ownerMessage("100")], options);
  const [socket] = world.sockets;
  if (!socket) throw new Error("gateway did not open");
  await world.connect(socket);
  const identify = socket.frames[0]?.d as { intents: number; presence?: unknown; token: string };
  expect(identify.intents).toBe((1 << 9) | (1 << 12) | (1 << 15));
  expect(identify.presence).toBeUndefined();
  expect(world.received).toEqual([]); // Cold start skips history, including the latest owner message.
  const requests = world.rest.requests.length;
  await world.dispatch(socket, ownerMessage("101"));
  expect(world.received).toEqual(["20:101"]);
  expect(world.rest.requests).toHaveLength(requests); // Delivered from the dispatch, not from a REST read.
  expect(world.cursor("20")).toBe("101");
  expect(world.logs.join("\n")).not.toContain("unit-private-token");
});

test("non-owner, bot, webhook, system and unwatched-channel messages are ignored but advance the cursor", async () => {
  const world = await liveWorld([], options, { "20": "100" });
  const [socket] = world.sockets;
  if (!socket) throw new Error("gateway did not open");
  await world.connect(socket);
  await world.dispatch(socket, ownerMessage("101", { author: { id: "11" } }));
  await world.dispatch(socket, ownerMessage("102", { author: { id: "10", bot: true } }));
  await world.dispatch(socket, ownerMessage("103", { author: { id: "99", bot: true } }));
  await world.dispatch(socket, ownerMessage("104", { webhook_id: "30" }));
  await world.dispatch(socket, ownerMessage("105", { type: 7 }));
  await world.dispatch(socket, ownerMessage("106", { channel_id: "21" }));
  expect(world.received).toEqual([]);
  expect(world.cursor("20")).toBe("105");
  await world.dispatch(socket, ownerMessage("107", { type: 19, message_reference: { message_id: "101" } }));
  expect(world.received).toEqual(["20:107"]);
});

test("READY backfills what was posted before the connection and a later live copy is not repeated", async () => {
  const world = await liveWorld([ownerMessage("100")], options);
  world.rest.state.messages.push(ownerMessage("101"), ownerMessage("102"));
  const [socket] = world.sockets;
  if (!socket) throw new Error("gateway did not open");
  await world.connect(socket);
  expect(world.received).toEqual(["20:101", "20:102"]);
  await world.dispatch(socket, ownerMessage("102")); // The same message arriving over both paths.
  await world.dispatch(socket, ownerMessage("101"));
  expect(world.received).toEqual(["20:101", "20:102"]);
  expect(world.cursor("20")).toBe("102");
});

test("a message sent during an outage is backfilled once after RESUMED, even when the gateway replays it", async () => {
  const world = await liveWorld([ownerMessage("100")], options);
  const [first] = world.sockets;
  if (!first) throw new Error("gateway did not open");
  await world.connect(first);
  await world.dispatch(first, ownerMessage("101"));
  first.disconnect(1006);
  world.rest.state.messages.push(ownerMessage("102"), ownerMessage("103"));
  world.advance(10_000);
  const second = world.sockets[1];
  if (!second) throw new Error("gateway did not reconnect");
  await world.connect(second, true);
  expect(second.frames[0]?.op).toBe(6);
  expect(world.received).toEqual(["20:101", "20:102", "20:103"]);
  await world.dispatch(second, ownerMessage("103")); // Replayed by the resumed session.
  await world.dispatch(second, ownerMessage("104"));
  expect(world.received).toEqual(["20:101", "20:102", "20:103", "20:104"]);
  expect(world.logs.some((line) => line.startsWith("DISCORD_GATEWAY_RECONNECT discord reason=closed code=1006"))).toBe(
    true,
  );
});

test("a terminal close code ends intake with an actionable error and does not retry", async () => {
  const world = await liveWorld([], options, { "20": "100" });
  const [socket] = world.sockets;
  if (!socket) throw new Error("gateway did not open");
  const outcome = world.live.done.then(
    () => "resolved",
    (error: Error) => error.message,
  );
  socket.hello();
  socket.disconnect(4014);
  expect(await outcome).toBe("Discord gateway closed code=4014 (disallowed intents)");
  world.advance(600_000);
  expect(world.sockets).toHaveLength(1);
});

test("a failing emitter stops intake: nothing queued behind it is delivered and its cursor is not advanced", async () => {
  let calls = 0;
  const world = await liveWorld([], options, { "20": "100" }, () => {
    if (++calls === 1) throw new Error("delivery failed");
  });
  const [socket] = world.sockets;
  if (!socket) throw new Error("gateway did not open");
  const outcome = world.live.done.then(
    () => "resolved",
    (error: Error) => error.message,
  );
  await world.connect(socket);
  socket.receive({ op: 0, t: "MESSAGE_CREATE", s: 5, d: ownerMessage("101") });
  socket.receive({ op: 0, t: "MESSAGE_CREATE", s: 6, d: ownerMessage("102") }); // Already queued behind the failure.
  expect(await outcome).toBe("delivery failed");
  await settle();
  expect(world.received).toEqual([]);
  expect(world.cursor("20")).toBe("100");
  expect(socket.closed).toBe(true);
});

test("a rate-limited or failing backfill is retried, and live messages wait behind it so none is skipped", async () => {
  const world = await liveWorld([ownerMessage("100")], options);
  world.rest.state.messages.push(ownerMessage("101"), ownerMessage("102"));
  world.rest.state.failure.set("/channels/20/messages", 429);
  const [socket] = world.sockets;
  if (!socket) throw new Error("gateway did not open");
  const outcome = world.live.done.then(
    () => "ended",
    (error: Error) => error.message,
  );
  await world.connect(socket);
  await world.dispatch(socket, ownerMessage("103"));
  expect(world.received).toEqual([]);
  expect(world.logs).toContain("DISCORD_GATEWAY_BACKFILL_RETRY discord retry_in=2s");
  world.rest.state.failure.clear();
  world.advance(2_000);
  await settle();
  expect(world.received).toEqual(["20:101", "20:102", "20:103"]);
  world.live.close();
  expect(await outcome).toBe("ended");
});
