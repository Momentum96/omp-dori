import { expect, test } from "bun:test";
import { Discord, type DcMessage } from "../src/messenger/discord.ts";
import { type DcCursor, snowflakeAt } from "../src/messenger/discord-intake-cursor.ts";
import {
  type DcInbound,
  type DcInboundOptions,
  DiscordIntake,
  formatDiscordIntake,
} from "../src/messenger/discord-intake.ts";
import { restFake } from "./discord-fakes.ts";
import { fakeClock } from "./fakes.ts";

const OWNER = "1197460639967359091";
const GUILD = "1570779238913671208";
const TEXT = "1570779429763027068";
const FORUM = "1570779431675494451";
const POST = "1570800000000000001";
const DM = "1570700000000000009";
const T0 = Date.parse("2026-11-13T12:00:00Z");

const memStore = (start: DcCursor = { channels: {} }) => {
  let saved: DcCursor = structuredClone(start);
  return {
    load: () => structuredClone(saved),
    save: (c: DcCursor) => {
      saved = structuredClone(c);
    },
    get: () => saved,
  };
};

const rig = (opts: Partial<DcInboundOptions> = {}, routes: Record<string, unknown> = {}) => {
  const rest = restFake(routes);
  const lines: string[] = [];
  const store = memStore();
  const inbound = new DiscordIntake(
    new Discord(rest.http, fakeClock(0), "B"),
    store,
    { ownerId: OWNER, ...opts },
    (i: DcInbound) => lines.push(formatDiscordIntake(i)),
    () => {},
    () => T0,
  );
  inbound.learnFrom("GUILD_CREATE", {
    id: GUILD,
    channels: [
      { id: TEXT, type: 0, name: "도리" },
      { id: FORUM, type: 15, name: "auction" },
    ],
    threads: [{ id: POST, type: 11, parent_id: FORUM, name: "fix login" }],
  });
  return { inbound, lines, rest, store };
};

let n = 0;
const msg = (channel: string, text: string, patch: Partial<DcMessage> = {}): DcMessage => ({
  id: (BigInt(snowflakeAt(T0)) + BigInt(++n)).toString(),
  channel_id: channel,
  ...(channel === DM ? {} : { guild_id: GUILD }),
  type: 0,
  author: { id: OWNER },
  content: text,
  ...patch,
});

test("only the owner's human messages pass, each printed with Telegram's field order", async () => {
  const { inbound, lines } = rig();
  const dm = msg(DM, "status?");
  const chan = msg(TEXT, 'say "hi"\nthen go');
  const post = msg(POST, "옥션 상태");
  await inbound.message(dm);
  await inbound.message(chan);
  await inbound.message(post);
  await inbound.message(msg(TEXT, "stranger", { author: { id: "42" } }));
  await inbound.message(msg(TEXT, "a bot with the owner's id", { author: { id: OWNER, bot: true } }));
  await inbound.message(msg(TEXT, "webhook", { webhook_id: "77" }));
  await inbound.message(msg(POST, "", { type: 18 })); // thread-created system message
  expect(lines).toEqual([
    `INBOUND dm ${DM} - ${dm.id} mid=${dm.id} ${OWNER} "status?"`,
    `INBOUND channel ${TEXT} - ${chan.id} mid=${chan.id} ${OWNER} "say \\"hi\\"\\nthen go"`,
    `INBOUND thread ${FORUM} ${POST} ${post.id} mid=${post.id} ${OWNER} "옥션 상태"`,
  ]);
});

test("the guild and channel allow-lists keep a forum's posts and drop other channels and guilds; DMs always pass", async () => {
  const { inbound, lines } = rig(
    { guildId: GUILD, channelIds: [FORUM] },
    { "GET /channels/1570900000000000005": { id: "1570900000000000005", type: 0, guild_id: "999" } },
  );
  await inbound.message(msg(TEXT, "not on the list"));
  await inbound.message(msg(POST, "on the list"));
  await inbound.message(msg(DM, "dm"));
  await inbound.message(msg("1570900000000000005", "other guild", { guild_id: "999" }));
  expect(lines.map((l) => l.split(" ").slice(1, 4).join(" "))).toEqual([`thread ${FORUM} ${POST}`, `dm ${DM} -`]);
});

test("attachments surface as file=<kind>:<url>, a voice message as voice so transcribe can fetch it", async () => {
  const { inbound, lines } = rig();
  const voiceUrl = "https://cdn.discordapp.com/attachments/1/2/voice-message.ogg?ex=1&is=2&hm=3";
  await inbound.message(
    msg(TEXT, "", {
      flags: 1 << 13,
      attachments: [{ url: voiceUrl, content_type: "audio/ogg", waveform: "AAAA" }],
    }),
  );
  await inbound.message(
    msg(TEXT, "two files", {
      attachments: [
        { url: "https://cdn.example/a.png", content_type: "image/png" },
        { url: "https://cdn.example/b.pdf", content_type: "application/pdf" },
      ],
    }),
  );
  expect(lines[0]).toEndWith(` "" file=voice:${voiceUrl}`);
  expect(lines[1]).toEndWith(
    ` "two files" file=photo:https://cdn.example/a.png file=document:https://cdn.example/b.pdf`,
  );
});

test("an edit prints once as edit:<source> with its own event id and the original mid; embed updates do not", async () => {
  const { inbound, lines } = rig();
  const original = msg(POST, "typo");
  await inbound.message(original);
  const edited = { ...original, content: "fixed", edited_timestamp: "2026-11-13T12:05:00.000+00:00" };
  await inbound.edit(edited);
  await inbound.edit(edited); // a replay after resume, or an embed refresh of the edited message
  await inbound.edit({ ...msg(POST, "unfurled"), edited_timestamp: null });
  await inbound.edit({ ...edited, author: { id: "42" }, edited_timestamp: "2026-11-13T12:06:00Z" });
  const editId = snowflakeAt(Date.parse("2026-11-13T12:05:00Z"));
  expect(lines.slice(1)).toEqual([
    `INBOUND edit:thread ${FORUM} ${POST} ${editId} mid=${original.id} ${OWNER} "fixed"`,
  ]);
});

test("a message in a channel it has not seen looks the channel up once, then cursors past replays", async () => {
  const THREAD = "1570900000000000077";
  const { inbound, lines, rest, store } = rig(
    {},
    { [`GET /channels/${THREAD}`]: { id: THREAD, type: 11, parent_id: TEXT, guild_id: GUILD } },
  );
  const first = msg(THREAD, "new thread");
  await inbound.message(first);
  await inbound.message(msg(THREAD, "again"));
  await inbound.message(first); // the gateway replays it after a resume
  expect(rest.seen).toEqual([`GET /channels/${THREAD}`]);
  expect(lines.map((l) => l.split(" ")[1])).toEqual(["thread", "thread"]);
  expect(store.get().channels[THREAD]).toBe((BigInt(first.id) + 1n).toString());
});
