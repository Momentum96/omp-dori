import { expect, test } from "bun:test";
import { discordInbound, formatDiscordInbound } from "../src/messenger/discord-inbound.ts";
import { MessengerError } from "../src/messenger/http.ts";
import {
  discordRestWorld,
  memoryDiscordCursor,
  ownerMessage,
  type TestDiscordMessage,
} from "./discord-inbound-fakes.ts";
import { liveWorld } from "./discord-inbound-live-fakes.ts";

const options = { ownerId: "10", channelIds: ["20"] };
const cdn = (name: string) => `https://cdn.discordapp.com/attachments/20/1/${name}?ex=1&is=2&hm=3`;
/** Discord's attachment object, including fields the intake does not carry (id, proxy_url, waveform). */
const attachment = (filename: string, patch: Record<string, unknown> = {}) => ({
  id: "500",
  filename,
  size: 1234,
  url: cdn(filename),
  proxy_url: cdn(`proxy-${filename}`),
  ...patch,
});
const voice = ownerMessage("103", {
  content: "",
  flags: 1 << 13,
  attachments: [attachment("voice-message.ogg", { content_type: "audio/ogg", duration_secs: 3.5, waveform: "AAAA" })],
});
const media: TestDiscordMessage[] = [
  ownerMessage("101", { content: "", attachments: [attachment("song.mp3", { content_type: "audio/mpeg" })] }),
  ownerMessage("102", {
    content: "two files",
    attachments: [attachment("a.png", { content_type: "image/png" }), attachment("notes")],
  }),
  voice,
  ownerMessage("104", {
    type: 19,
    content: "this one",
    message_reference: { message_id: "90" },
    referenced_message: { id: "90", author: { id: "99" }, content: 'bot said "hi"' },
  }),
  ownerMessage("105", { type: 19, content: "gone", message_reference: { message_id: "91" }, referenced_message: null }),
  ownerMessage("106", { author: { id: "11" }, attachments: [attachment("private.png")] }),
];
const expected = [
  'INBOUND discord 20 101 from=owner ref=- ""',
  `INBOUND_ATTACHMENT discord 20 101 {"filename":"song.mp3","content_type":"audio/mpeg","size":1234,"url":"${cdn("song.mp3")}"}`,
  'INBOUND discord 20 102 from=owner ref=- "two files"',
  `INBOUND_ATTACHMENT discord 20 102 {"filename":"a.png","content_type":"image/png","size":1234,"url":"${cdn("a.png")}"}`,
  `INBOUND_ATTACHMENT discord 20 102 {"filename":"notes","size":1234,"url":"${cdn("notes")}"}`,
  'INBOUND discord 20 103 from=owner ref=- ""',
  `INBOUND_ATTACHMENT discord 20 103 {"filename":"voice-message.ogg","content_type":"audio/ogg","size":1234,"url":"${cdn("voice-message.ogg")}","voice":true,"duration_secs":3.5}`,
  'INBOUND discord 20 104 from=owner ref=90 "this one"',
  'INBOUND_REPLY discord 20 104 {"id":"90","author":"99","text":"bot said \\"hi\\""}',
  'INBOUND discord 20 105 from=owner ref=91 "gone"',
];

/** One REST poll over the media history, as the printed output lines. */
const restLines = async () => {
  const world = discordRestWorld(media);
  const cursor = memoryDiscordCursor("100");
  const intake = await discordInbound(world.discord, () => cursor.store, options);
  const lines: string[] = [];
  await intake.poll((message) => lines.push(formatDiscordInbound(message)));
  return { lines: lines.join("\n").split("\n"), cursor: cursor.store.load() };
};

test("attachments, voice messages and replies follow their INBOUND line in REST polling", async () => {
  expect(await restLines()).toEqual({ lines: expected, cursor: "106" });
});

test("the Gateway delivers the same lines for the same messages", async () => {
  const world = await liveWorld([], options, { "20": "100" });
  const [socket] = world.sockets;
  if (!socket) throw new Error("gateway did not open");
  await world.connect(socket);
  for (const message of media) await world.dispatch(socket, message);
  expect(world.lines.join("\n").split("\n")).toEqual(expected);
  expect(world.cursor("20")).toBe("106");
});

test("a parser of the documented INBOUND line reads media messages unchanged and never matches the extra records", async () => {
  const grammar = /^INBOUND discord (\d+) (\d+) from=owner ref=(\d+|-) (".*")$/;
  const { lines } = await restLines();
  const parsed = lines.flatMap((line) => {
    const match = grammar.exec(line);
    return match ? [[match[2], match[3], JSON.parse(match[4] ?? "")]] : [];
  });
  expect(parsed).toEqual([
    ["101", "-", ""],
    ["102", "-", "two files"],
    ["103", "-", ""],
    ["104", "90", "this one"],
    ["105", "91", "gone"],
  ]);
  const extra = lines.filter((line) => !grammar.test(line));
  expect(extra).toHaveLength(5);
  expect(extra.some((line) => line.startsWith("INBOUND discord "))).toBe(false);
  for (const line of extra) JSON.parse(line.split(" ").slice(4).join(" "));
});

test("a malformed attachment stops intake without advancing past the message", async () => {
  const world = discordRestWorld([
    ownerMessage("101"),
    ownerMessage("102", { attachments: [{ filename: "x.png", size: 1 }] }),
  ]);
  const cursor = memoryDiscordCursor("100");
  const intake = await discordInbound(world.discord, () => cursor.store, options);
  const received: string[] = [];
  const failure = intake.poll((message) => received.push(message.message));
  await expect(failure).rejects.toThrow(MessengerError);
  await expect(failure).rejects.toThrow("Discord inbound attachment is invalid");
  expect(received).toEqual(["101"]);
  expect(cursor.store.load()).toBe("101");
});
