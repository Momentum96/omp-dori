/** dori inbound/send/thread/react discord, and the attachment download behind dori transcribe <url>. */
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { type Cli, type Command, die } from "./cli-context.ts";
import { type DcChannel, Discord, isForumType } from "./messenger/discord.ts";
import { fileDcStore, parseSnowflake } from "./messenger/discord-intake-cursor.ts";
import { DISCORD_INBOUND_INTENTS, type DiscordSocket, runDiscordGateway } from "./messenger/discord-intake-gateway.ts";
import { DiscordIntake, formatDiscordIntake } from "./messenger/discord-intake.ts";
import { discordInboundLoop } from "./messenger/discord-intake-loop.ts";
import { fetchHttp } from "./messenger/http.ts";
import type { Timers } from "./messenger/typing.ts";
import { realClock } from "./run.ts";

const realTimers: Timers = {
  setInterval: (cb, ms) => setInterval(cb, ms),
  clearInterval: (h) => clearInterval(h as ReturnType<typeof setInterval>),
};

const env = (name: string): string => process.env[name] ?? die(`${name} is not set`);
const id = (name: string, raw: string | undefined): string => {
  try {
    return parseSnowflake(name, raw);
  } catch (e) {
    return die(e instanceof Error ? e.message : String(e));
  }
};
const client = () => new Discord(fetchHttp, realClock, env("DORI_DISCORD_TOKEN"));
const warn = (m: string) => console.log(`WARN ${m}`.slice(0, 300));

/** A local path as is; a URL (a Discord INBOUND file= attachment) fetched to a private temp file for `use`. */
export const withLocalAudio = async (source: string, use: (file: string) => Promise<string>): Promise<string> => {
  if (!/^https?:\/\//.test(source)) return await use(source);
  const res = await fetch(source);
  if (!res.ok) die(`transcribe: GET ${new URL(source).pathname} answered ${res.status}`);
  const dir = mkdtempSync(join(tmpdir(), "dori-audio-"));
  try {
    const file = join(dir, basename(new URL(source).pathname) || "audio");
    writeFileSync(file, new Uint8Array(await res.arrayBuffer()));
    return await use(file);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
};

const openSocket = (url: string): DiscordSocket => {
  const ws = new WebSocket(url);
  let onMessage: (d: string) => void = () => {};
  let onClose: (code: number) => void = () => {};
  ws.addEventListener("message", (e) => onMessage(String(e.data)));
  ws.addEventListener("close", (e) => onClose(e.code));
  return {
    send: (d) => ws.send(d),
    close: (code) => ws.close(code),
    onMessage: (cb) => {
      onMessage = cb;
    },
    onClose: (cb) => {
      onClose = cb;
    },
  };
};

export const discordGatewayInbound = async (stateDir: string, minutes: number): Promise<void> => {
  const dc = client();
  const ownerId = id("DORI_DISCORD_OWNER_ID", process.env["DORI_DISCORD_OWNER_ID"]);
  const rawGuild = process.env["DORI_DISCORD_GUILD_ID"];
  const guildId = rawGuild === undefined ? undefined : id("DORI_DISCORD_GUILD_ID", rawGuild);
  const rawChannels = process.env["DORI_DISCORD_CHANNEL_IDS"];
  const channelIds = rawChannels?.split(",").map((c) => id("DORI_DISCORD_CHANNEL_IDS", c));
  const inbound = new DiscordIntake(
    dc,
    fileDcStore(`${stateDir}/discord-cursor.json`, warn),
    { ownerId, ...(guildId === undefined ? {} : { guildId }), ...(channelIds === undefined ? {} : { channelIds }) },
    (i) => console.log(formatDiscordIntake(i)),
    warn,
    Date.now,
  );
  const token = env("DORI_DISCORD_TOKEN");
  await discordInboundLoop(inbound, minutes, {
    gateway: (hooks) =>
      runDiscordGateway(
        token,
        DISCORD_INBOUND_INTENTS,
        { open: openSocket, timers: realTimers, sleep: (ms) => Bun.sleep(ms), now: Date.now, warn },
        hooks,
      ),
    sleep: (ms) => Bun.sleep(ms),
    now: Date.now,
    ready: (mode) =>
      console.log(
        `INBOUND_READY discord owner=${ownerId} guild=${guildId ?? "-"} channels=${channelIds?.join(",") ?? "-"} mode=${mode}`,
      ),
    warn,
  });
};

/** A channel id, or a channel name in DORI_DISCORD_GUILD_ID (`auction`, `#auction`). */
const channelOf = async (dc: Discord, raw: string): Promise<DcChannel> => {
  if (/^\d+$/.test(raw)) return await dc.channel(id("--to", raw));
  const guild = id("DORI_DISCORD_GUILD_ID (to look up a channel by name)", process.env["DORI_DISCORD_GUILD_ID"]);
  const name = raw.replace(/^#/, "");
  const hits = (await dc.guildChannels(guild)).filter((c) => c.name === name);
  if (hits.length !== 1) return die(`--to ${raw}: ${hits.length} channels have that name in guild ${guild}`);
  return hits[0]!;
};

/** Forum tag names (or ids) to tag ids; an unknown name is refused with the tags the forum has. */
const tagIds = (forum: DcChannel, names: readonly string[]): string[] =>
  names.map((n) => {
    const tags = forum.available_tags ?? [];
    return (
      tags.find((t) => t.name === n || t.id === n)?.id ??
      die(`forum ${forum.id} has no tag ${JSON.stringify(n)}; tags: ${tags.map((t) => t.name).join(", ") || "none"}`)
    );
  });

export const sendDiscord = async ({ need, opt }: Cli): Promise<void> => {
  const dc = client();
  const text = need("text");
  const thread = opt("thread");
  const edit = opt("edit");
  let target: string;
  if (thread !== undefined) target = id("--thread", thread);
  else {
    const ch = await channelOf(dc, need("to"));
    if (isForumType(ch.type)) die(`${ch.id} is a forum: post with dori thread discord create, or pass --thread`);
    target = ch.id;
  }
  if (edit !== undefined) await dc.edit(target, id("--edit", edit), text);
  else console.log(`SENT ${await dc.send(target, text)}`);
};

/**
 * `thread discord create --to CHANNEL --title T [--text T] [--tag NAME]...`: a forum post (tags applied) or a
 * thread in a text channel. `thread discord set <thread> [--title T] [--tag NAME]... [--archive|--unarchive]`:
 * rename, replace the post's tags, archive. Tags resolve by name against the parent forum.
 */
export const threadDiscord: Command = async ({ positionals, need, opt, strings, values }) => {
  const dc = client();
  const action = positionals[1];
  const tags = strings("tag");
  if (action === "create") {
    const ch = await channelOf(dc, need("to"));
    if (tags.length && !isForumType(ch.type)) die("--tag needs a forum channel");
    const text = opt("text");
    const t = await dc.createThread(ch, need("title"), {
      ...(text === undefined ? {} : { text }),
      ...(tags.length ? { appliedTags: tagIds(ch, tags) } : {}),
    });
    console.log(`THREAD ${t.id}${t.messageId === undefined ? "" : ` mid=${t.messageId}`}`);
  } else if (action === "set") {
    const threadId = id("thread", positionals[2]);
    const title = opt("title");
    let appliedTags: string[] | undefined;
    if (tags.length) {
      const parent = (await dc.channel(threadId)).parent_id ?? die(`${threadId} is not a thread`);
      const forum = await dc.channel(parent);
      if (!isForumType(forum.type)) die("--tag needs a forum post");
      appliedTags = tagIds(forum, tags);
    }
    const archived = values["archive"] === true ? true : values["unarchive"] === true ? false : undefined;
    if (title === undefined && appliedTags === undefined && archived === undefined)
      die("thread set needs --title, --tag, --archive or --unarchive");
    await dc.setThread(threadId, {
      ...(title === undefined ? {} : { name: title }),
      ...(appliedTags === undefined ? {} : { appliedTags }),
      ...(archived === undefined ? {} : { archived }),
    });
    console.log(`THREAD ${threadId}`);
  } else die("thread discord needs create or set");
};

/** `react discord --to CHANNEL [--thread ID] --message ID [--emoji E] [--remove]`; the default 👀 is a read receipt. */
export const reactDiscord: Command = async ({ need, opt, values }) => {
  const thread = opt("thread");
  const channel = thread === undefined ? id("--to", need("to")) : id("--thread", thread);
  await client().react(channel, id("--message", need("message")), opt("emoji") || "👀", values["remove"] === true);
  console.log(`REACTED ${channel} ${values["remove"] === true ? "-" : "+"}`);
};
