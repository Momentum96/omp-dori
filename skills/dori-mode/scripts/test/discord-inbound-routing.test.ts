import { expect, test } from "bun:test";
import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { basename, join } from "node:path";
import { discordInbound, formatDiscordInbound } from "../src/messenger/discord-inbound.ts";
import { type Bot, routeConfigFromTopology, snowflakeMs, type ThreadInfo } from "../src/messenger/discord-route.ts";
import { discordRestWorld, memoryDiscordCursor, type TestDiscordMessage } from "./discord-inbound-fakes.ts";

type Step = { message: TestDiscordMessage & { edited_timestamp?: string }; thread?: ThreadInfo; event?: string };
type Put = { kind: string; reason: string };
type Vector = {
  id: string;
  bots: Bot[];
  gateway?: boolean;
  steps: Step[];
  expect: { addressed?: Bot[][]; puts?: Partial<Record<Bot, Put[]>> };
};

// Vendored from dori-shared (node/routing-vectors.json, node/discord-watch/test-topology.json) so every CI runs the
// shared rules; the drift tests compare them with dori-shared wherever a checkout exists (DORI_SHARED_NODE overrides).
const fixtures = join(import.meta.dir, "fixtures");
const vectorsPath = join(fixtures, "routing-vectors.json");
const topologyPath = join(fixtures, "routing-topology.json");
const sharedNode = process.env["DORI_SHARED_NODE"] || join(homedir(), ".dori/shared/node");
const sharedCopies = [
  [vectorsPath, join(sharedNode, "routing-vectors.json")],
  [topologyPath, join(sharedNode, "discord-watch/test-topology.json")],
] as const;
for (const [vendored, shared] of sharedCopies) {
  test.skipIf(!existsSync(shared))(`${basename(vendored)} is identical to ${shared}`, () => {
    expect(readFileSync(vendored, "utf8")).toBe(readFileSync(shared, "utf8"));
  });
}

// Vector attachments carry only a URL; Discord always adds filename and size, which this receiver validates.
const discordShape = (message: Step["message"]): Step["message"] =>
  Array.isArray(message.attachments)
    ? { ...message, attachments: message.attachments.map((file: object) => ({ filename: "file", size: 1, ...file })) }
    : message;
const raw: Vector[] = JSON.parse(readFileSync(vectorsPath, "utf8"));
const vectors = raw.map((vector) => ({
  ...vector,
  steps: vector.steps.map((step) => ({ ...step, message: discordShape(step.message) })),
}));
const config = routeConfigFromTopology(JSON.parse(readFileSync(topologyPath, "utf8")));
const homes = [...config.topicChannels, ...config.testChannels];
const lastMs = (vector: Vector) => Math.max(...vector.steps.map((step) => snowflakeMs(step.message.id)));

/** One omp intake over the vector's channels and threads; `listed` makes the startup listing find the threads. */
const intakeFor = async (vector: Vector, listed: boolean) => {
  const rest = discordRestWorld(vector.steps.filter((step) => step.event !== "update").map((step) => step.message));
  rest.state.botId = config.bots.omp.userId;
  const threads = new Map(
    vector.steps.flatMap((step) => (step.thread ? [[step.message.channel_id, step.thread]] : [])),
  );
  for (const [id, thread] of threads) {
    rest.state.threadParents.set(id, thread.parentId ?? "0");
    if (thread.ownerId) rest.state.threadOwners.set(id, thread.ownerId);
    rest.state.starters.set(id, thread.starterAuthorId);
  }
  rest.state.guilds = [{ id: "800000000000000001", channels: homes, threads: listed ? [...threads.keys()] : [] }];
  let now = 0;
  const stores = new Map<string, ReturnType<typeof memoryDiscordCursor>>();
  const storeFor = (channel: string) => {
    const entry = stores.get(channel) ?? memoryDiscordCursor("1");
    stores.set(channel, entry);
    return entry.store;
  };
  const routing = { bot: "omp" as const, config, now: () => now };
  const intake = await discordInbound(rest.discord, storeFor, {
    ownerId: config.ownerId,
    channelIds: homes,
    routing,
  });
  const lines: string[] = [];
  const emit = (message: Parameters<typeof formatDiscordInbound>[0]) => lines.push(formatDiscordInbound(message));
  return { rest, intake, lines, emit, stores, at: (ms: number) => (now = ms), now: () => now };
};
type World = Awaited<ReturnType<typeof intakeFor>>;

/** Feeds the vector as live dispatches at each message's own time, firing held burst decisions when due. */
const replayLive = async (vector: Vector, world: World) => {
  for (const step of vector.steps) {
    const edited = step.message.edited_timestamp;
    const at = edited ? Date.parse(edited) : snowflakeMs(step.message.id) + 1;
    const wait = world.intake.wakeIn();
    if (wait !== undefined && world.now() + wait <= at) {
      world.at(world.now() + wait);
      await world.intake.settle(world.emit);
    }
    world.at(at);
    if (step.event === "update") {
      const { messages } = world.rest.state;
      world.rest.state.messages = messages.map((message) => (message.id === step.message.id ? step.message : message));
      const { id, channel_id, content } = step.message;
      await world.intake.update(
        { id, channel_id, ...(content === undefined ? {} : { content }), edited_timestamp: edited ?? null },
        world.emit,
      );
    } else await world.intake.accept(step.message, world.emit);
  }
  world.at(lastMs(vector) + 60_000);
  await world.intake.settle(world.emit);
};

const inbound = (lines: readonly string[]) =>
  lines.flatMap((line) => line.split("\n")).filter((line) => line.startsWith("INBOUND "));
/** Every watched channel's cursor ends at its newest message: dropped messages advance it too. */
const expectCursors = (vector: Vector, world: World) => {
  const newest = new Map<string, string>();
  for (const { message } of vector.steps) {
    const seen = newest.get(message.channel_id);
    if (seen === undefined || BigInt(message.id) > BigInt(seen)) newest.set(message.channel_id, message.id);
  }
  for (const channel of world.intake.channels()) {
    const id = newest.get(channel);
    if (id !== undefined) expect(world.stores.get(channel)?.store.load()).toBe(id);
  }
};

test("the vendored vector file covers the contract cases the omp receiver relies on", () => {
  const ids = new Set(vectors.map((vector) => vector.id));
  for (const id of ["T1.1", "T1.2", "T1.8", "T1.10", "T1.12", "T1.13a", "T1.13b", "TO.1", "TO.2", "E.1"]) {
    expect(ids.has(id)).toBe(true);
  }
});

test.skipIf(!existsSync(join(sharedNode, "discord-watch/route.ts")))(
  "discord-route.ts is a verbatim copy of the shared route.ts",
  () => {
    const copy = readFileSync(join(import.meta.dir, "../src/messenger/discord-route.ts"), "utf8");
    expect(copy.slice(copy.indexOf("\n") + 1)).toBe(readFileSync(join(sharedNode, "discord-watch/route.ts"), "utf8"));
  },
);

for (const vector of vectors.filter((v) => !v.gateway && v.bots.includes("omp"))) {
  const addressed = vector.expect.addressed ?? [];
  const expected = vector.steps.filter((_, i) => addressed[i]?.includes("omp")).map((step) => step.message.id);
  test(`omp ${vector.id} from history: one INBOUND per message that addresses omp, none otherwise`, async () => {
    const world = await intakeFor(vector, true);
    world.at(lastMs(vector) + 60_000);
    await world.intake.poll(world.emit);
    expect(inbound(world.lines).map((line) => line.split(" ")[3])).toEqual(expected);
    expectCursors(vector, world);
  });
  test(`omp ${vector.id} live: the same decisions from gateway dispatches on a fake clock`, async () => {
    const world = await intakeFor(vector, false);
    await replayLive(vector, world);
    expect(inbound(world.lines).map((line) => line.split(" ")[3])).toEqual(expected);
    expect(world.intake.wakeIn()).toBeUndefined();
    expectCursors(vector, world);
  });
}

for (const vector of vectors.filter((v) => v.gateway && v.bots.includes("omp"))) {
  test(`omp ${vector.id} gateway edits: only an edit that newly addresses omp becomes a request`, async () => {
    const puts = vector.expect.puts?.omp ?? [];
    expect(puts.every((put) => put.kind === "request")).toBe(true); // Other edit kinds are omo-only vectors.
    const world = await intakeFor(vector, false);
    await replayLive(vector, world);
    const lines = world.lines.flatMap((line) => line.split("\n"));
    expect(lines.filter((line) => line.startsWith("INBOUND "))).toHaveLength(puts.length);
    const edits = lines.filter((line) => line.startsWith("INBOUND_EDIT "));
    expect(edits).toHaveLength(puts.filter((put) => put.reason === "edit-added").length);
    for (const line of edits) expect(line).toMatch(/"id":"e:omp:\d+:\d+"/);
  });
}
