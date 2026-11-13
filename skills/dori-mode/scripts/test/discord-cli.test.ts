import { afterAll, expect, test } from "bun:test";
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { run } from "../src/run.ts";
import { withState } from "./fakes.ts";

const cli = join(import.meta.dir, "../src/cli.ts");
const preload = join(import.meta.dir, "record-discord.ts");
const OWNER = "1197460639967359091";
const GUILD = "1570779238913671208";
const TEXT = "1570779429763027068";
const FORUM = "1570779431675494451";
const POST = "1570800000000000001";
const MID = "1580000000000000099";
const TAGS = [
  { id: "1570779431675494460", name: "작업중" },
  { id: "1570779431675494461", name: "대기" },
  { id: "1570779431675494462", name: "완료" },
];

const state = withState();
afterAll(() => state.done());
const log = join(state.dir, "fetch.jsonl");
const config = join(state.dir, "config.json");
writeFileSync(
  config,
  JSON.stringify({ stateDir: state.dir, defaultCwd: state.dir, hooks: { transcribe: ["cat", "{file}"] } }),
);
writeFileSync(
  join(state.dir, "routes.json"),
  JSON.stringify({
    [`GET /channels/${TEXT}`]: { id: TEXT, type: 0, guild_id: GUILD, name: "도리" },
    [`GET /channels/${FORUM}`]: { id: FORUM, type: 15, guild_id: GUILD, name: "auction", available_tags: TAGS },
    [`GET /channels/${POST}`]: { id: POST, type: 11, guild_id: GUILD, parent_id: FORUM },
    [`GET /guilds/${GUILD}/channels`]: [
      { id: TEXT, type: 0, name: "도리" },
      { id: FORUM, type: 15, name: "auction" },
    ],
    [`POST /channels/${TEXT}/messages`]: { id: MID },
    [`PATCH /channels/${POST}/messages/${MID}`]: { id: MID },
    [`POST /channels/${FORUM}/threads`]: { id: POST },
    [`PATCH /channels/${POST}`]: { id: POST },
    "GET /attachments/1/2/voice-message.ogg?ex=1": "audio bytes",
    [`PUT /channels/${POST}/messages/${MID}/reactions/%F0%9F%91%80/@me`]: null,
  }),
);

const dori = async (args: string[], extraEnv: string[] = []) => {
  rmSync(log, { force: true });
  const r = await run(
    [
      "env",
      // A set DORI_DISCORD_CHANNEL_ID selects the single-channel REST intake instead of the gateway one.
      "-u",
      "DORI_DISCORD_CHANNEL_ID",
      `DORI_CONFIG=${config}`,
      `DORI_STATE_DIR=${state.dir}`,
      "DORI_DISCORD_TOKEN=synthetic",
      `DORI_TEST_FETCH_LOG=${log}`,
      `DORI_TEST_DISCORD_ROUTES=${join(state.dir, "routes.json")}`,
      ...extraEnv,
      process.execPath,
      "--preload",
      preload,
      cli,
      ...args,
    ],
    { timeoutMs: 20_000 },
  );
  const requests = existsSync(log)
    ? readFileSync(log, "utf8")
        .trim()
        .split("\n")
        .map((l) => JSON.parse(l) as { key?: string; body?: Record<string, unknown>; ws?: { op: number } })
    : [];
  return { ...r, requests, keys: requests.flatMap((q) => (q.key ? [q.key] : [])) };
};

test("send discord posts to a channel without pings, edits inside a thread, and refuses a forum or a bad id", async () => {
  const sent = await dori(["send", "discord", "--to", TEXT, "--text", "hi @everyone"]);
  expect([sent.code, sent.out, sent.keys]).toEqual([
    0,
    `SENT ${MID}`,
    [`GET /channels/${TEXT}`, `POST /channels/${TEXT}/messages`],
  ]);
  expect(sent.requests[1]!.body).toEqual({ content: "hi @everyone", allowed_mentions: { parse: [] } });
  const edited = await dori(["send", "discord", "--to", TEXT, "--thread", POST, "--edit", MID, "--text", "v2"]);
  expect([edited.code, edited.keys]).toEqual([0, [`PATCH /channels/${POST}/messages/${MID}`]]);
  const forum = await dori(["send", "discord", "--to", "#auction", "--text", "x"], [`DORI_DISCORD_GUILD_ID=${GUILD}`]);
  expect([forum.code, forum.err]).toEqual([
    1,
    `${FORUM} is a forum: post with dori thread discord create, or pass --thread`,
  ]);
  const bad = await dori(["send", "discord", "--thread", "12ab", "--text", "x"]);
  expect([bad.code, bad.err, bad.keys]).toEqual([1, '--thread must be a Discord id (digits only), got: "12ab"', []]);
}, 60_000);

test("thread discord opens a tagged forum post, then renames, retags and archives it by tag name", async () => {
  const made = await dori([
    "thread",
    "discord",
    "create",
    "--to",
    FORUM,
    "--title",
    "옥션: fix login",
    "--tag",
    "작업중",
  ]);
  expect([made.code, made.out]).toEqual([0, `THREAD ${POST} mid=${POST}`]);
  expect(made.requests[1]!.body).toEqual({
    name: "옥션: fix login",
    message: { content: "옥션: fix login", allowed_mentions: { parse: [] } },
    applied_tags: [TAGS[0]!.id],
  });
  const set = await dori([
    "thread",
    "discord",
    "set",
    POST,
    "--title",
    "[완료] fix login",
    "--tag",
    "완료",
    "--archive",
  ]);
  expect([set.code, set.keys]).toEqual([
    0,
    [`GET /channels/${POST}`, `GET /channels/${FORUM}`, `PATCH /channels/${POST}`],
  ]);
  expect(set.requests[2]!.body).toEqual({ name: "[완료] fix login", archived: true, applied_tags: [TAGS[2]!.id] });
  const unknown = await dori(["thread", "discord", "set", POST, "--tag", "done"]);
  expect([unknown.code, unknown.err]).toEqual([1, `forum ${FORUM} has no tag "done"; tags: 작업중, 대기, 완료`]);
}, 60_000);

test("react discord adds the 👀 read receipt on a thread message", async () => {
  const r = await dori(["react", "discord", "--to", FORUM, "--thread", POST, "--message", MID]);
  expect([r.code, r.out, r.keys]).toEqual([
    0,
    `REACTED ${POST} +`,
    [`PUT /channels/${POST}/messages/${MID}/reactions/%F0%9F%91%80/@me`],
  ]);
}, 60_000);

test("inbound discord --loop prints one READY and the owner's gateway message; a fatal close exits naming it", async () => {
  const gateway = join(state.dir, "gateway.json");
  const message = (author: string, content: string, id: string) => ({
    op: 0,
    t: "MESSAGE_CREATE",
    d: { id, channel_id: TEXT, guild_id: GUILD, type: 0, author: { id: author }, content },
  });
  writeFileSync(
    gateway,
    JSON.stringify({
      frames: [
        { op: 10, d: { heartbeat_interval: 41_250 } },
        { op: 0, s: 1, t: "READY", d: { session_id: "S", resume_gateway_url: "wss://r" } },
        { op: 0, s: 2, t: "GUILD_CREATE", d: { id: GUILD, channels: [{ id: TEXT, type: 0 }], threads: [] } },
        { ...message("42", "stranger", "1580000000000000001"), s: 3 },
        { ...message(OWNER, "경매 결과 확인해줘", "1580000000000000002"), s: 4 },
      ],
      close: 4004,
    }),
  );
  const r = await dori(
    ["inbound", "discord", "--loop", "1"],
    [`DORI_DISCORD_OWNER_ID=${OWNER}`, `DORI_DISCORD_GUILD_ID=${GUILD}`, `DORI_TEST_GATEWAY=${gateway}`],
  );
  expect(r.code).toBe(1);
  expect(r.err).toBe("discord gateway closed 4004: authentication failed; check DORI_DISCORD_TOKEN");
  expect(r.out.split("\n")).toEqual([
    `INBOUND_READY discord owner=${OWNER} guild=${GUILD} channels=- mode=gateway`,
    `INBOUND channel ${TEXT} - 1580000000000000002 mid=1580000000000000002 ${OWNER} "경매 결과 확인해줘"`,
  ]);
  expect(r.requests.find((q) => q.ws?.op === 2)).toBeDefined();
  expect(JSON.parse(readFileSync(join(state.dir, "discord-cursor.json"), "utf8")).channels[TEXT]).toBe(
    "1580000000000000002",
  );
  const badOwner = await dori(["inbound", "discord"], ["DORI_DISCORD_OWNER_ID=1e3"]);
  expect([badOwner.code, badOwner.err]).toEqual([
    1,
    'DORI_DISCORD_OWNER_ID must be a Discord id (digits only), got: "1e3"',
  ]);
}, 60_000);

test("transcribe takes an INBOUND file= URL: it downloads to a private temp file, runs the hook, and removes it", async () => {
  const tmp = join(state.dir, "tmp");
  mkdirSync(tmp);
  const r = await dori(
    ["transcribe", "https://cdn.discordapp.com/attachments/1/2/voice-message.ogg?ex=1"],
    [`TMPDIR=${tmp}`],
  );
  expect([r.code, r.out, r.keys]).toEqual([0, '"audio bytes"', ["GET /attachments/1/2/voice-message.ogg?ex=1"]]);
  expect(readdirSync(tmp)).toEqual([]);
}, 60_000);
