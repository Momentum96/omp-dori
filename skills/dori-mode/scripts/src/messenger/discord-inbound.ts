import type { Discord } from "./discord.ts";
import {
  type DiscordInbound,
  discoverChannels,
  inboundProtocolError,
  inboundRequest,
  isOwnerMessage,
  type Message,
  messagePage,
  newer,
  toInbound,
  unavailable,
} from "./discord-inbound-api.ts";
import { type DiscordRouting, discordRouting } from "./discord-inbound-routing.ts";
import { discordInboundEnv, type DiscordInboundStore, isDiscordSnowflake } from "./discord-inbound-state.ts";
import { discordThreads } from "./discord-inbound-threads.ts";
import { MessengerError } from "./http.ts";

export type { DiscordInbound } from "./discord-inbound-api.ts";
export type DiscordInboundOptions = {
  ownerId: string;
  /** Channels and threads that must be reachable; a failure on one of them is an error. */
  channelIds: readonly string[];
  /** Also watch every text channel, active thread and the owner DM the bot can see; inaccessible ones are skipped. */
  all?: boolean;
  /** Watch active threads whose parent is a watched channel; set false to watch only the configured ids. */
  threads?: boolean;
  /** Mention routing: emit only messages that address this bot under the shared topology. Unset keeps owner-only intake. */
  routing?: DiscordRouting;
};

/** Formats one unchanged text line followed by reply and attachment records for the existing single emit. */
export const formatDiscordInbound = (message: DiscordInbound): string => {
  const id = `discord ${message.channel} ${message.message}`;
  return [
    `INBOUND ${id} from=owner ref=${message.reference ?? "-"} ${JSON.stringify(message.text)}`,
    ...(message.edit ? [`INBOUND_EDIT ${id} ${JSON.stringify(message.edit)}`] : []),
    ...(message.replyTo ? [`INBOUND_REPLY ${id} ${JSON.stringify(message.replyTo)}`] : []),
    ...message.attachments.map((attachment) => `INBOUND_ATTACHMENT ${id} ${JSON.stringify(attachment)}`),
  ].join("\n");
};

/**
 * A bot-authenticated Discord intake over any number of channels. Every channel owns a persisted cursor, and a
 * message is delivered only when its id is above that cursor, so the REST history path (`poll`) and live Gateway
 * delivery (`accept`) share one exactly-once, ascending-order guarantee. Presence remains a separate connection.
 */
export const discordInbound = async (
  discord: Discord,
  storeFor: (channelId: string) => DiscordInboundStore,
  opts: DiscordInboundOptions,
) => {
  const ownerId = discordInboundEnv("DORI_DISCORD_OWNER_ID", opts.ownerId);
  const explicit = opts.channelIds.map((id) => discordInboundEnv("DORI_DISCORD_CHANNEL_IDS", id));
  const request = inboundRequest(discord);
  const me = await request<{ id?: string; bot?: boolean }>("/users/@me");
  if (!me || me.bot !== true || !isDiscordSnowflake(me.id)) {
    throw new MessengerError("Discord inbound requires an authenticated bot identity; use a bot token", 401);
  }
  const botId = me.id;
  if (botId === ownerId)
    throw new MessengerError("DORI_DISCORD_OWNER_ID must identify the human owner, not the bot", 0);
  const routing = opts.routing;
  if (routing && (routing.config.bots[routing.bot].userId !== botId || routing.config.ownerId !== ownerId)) {
    throw new MessengerError(
      `Discord topology names another ${routing.bot} bot or owner than DORI_DISCORD_TOKEN and DORI_DISCORD_OWNER_ID`,
      0,
    );
  }
  const application = await request<{ flags?: number }>("/applications/@me");
  const flags = application?.flags;
  if (typeof flags !== "number" || !Number.isSafeInteger(flags) || (flags & ((1 << 18) | (1 << 19))) === 0) {
    throw new MessengerError(
      "Discord inbound needs Message Content Intent enabled for this bot in the Developer Portal",
      403,
    );
  }
  const watched = new Map<string, DiscordInboundStore>();
  /** Watched plain channels with the parent position captured when they were first attached (startup floor). */
  const parents = new Map<string, string>();
  const threads = opts.threads !== false && !opts.all; // Scope `all` already lists every active thread.
  /** Only history-read failures can make a discovered channel unavailable, never state or delivery failures. */
  const readPage = async (channelId: string, limit: number, before?: string) => {
    try {
      return await messagePage(request, channelId, limit, before);
    } catch (error) {
      if (explicit.includes(channelId) || !unavailable(error)) throw error;
      watched.delete(channelId);
      return undefined;
    }
  };
  /** Watches one channel; `seed` sets a cold-start cursor instead of the channel's latest message. */
  const attach = async (channelId: string, seed?: string): Promise<string | undefined> => {
    const store = storeFor(channelId);
    const cursor = store.load();
    const latest = await readPage(channelId, 1); // Also verifies channel access before readiness.
    if (latest === undefined) return undefined;
    if (latest.some((message) => message.channel_id !== channelId)) {
      throw inboundProtocolError("Discord inbound channel response does not match the configured channel");
    }
    if (cursor === undefined) store.save(seed ?? latest[0]?.id ?? "0"); // Cold start skips existing history.
    watched.set(channelId, store);
    return latest[0]?.id ?? "0";
  };
  const following = discordThreads({ request, explicit, watched, parents, attach });
  const gate = routing ? discordRouting(routing, request) : undefined;
  const refresh = async (initial: boolean): Promise<void> => {
    for (const id of explicit) {
      if (watched.has(id)) continue;
      const latest = await attach(id);
      if (latest !== undefined && threads) await following.register(id, latest);
    }
    if (opts.all) {
      for (const id of await discoverChannels(request, ownerId)) if (!watched.has(id)) await attach(id);
    }
    if (threads) await following.follow(initial);
  };
  const deliver = (message: Message, store: DiscordInboundStore, emit: (message: DiscordInbound) => void) => {
    if (isOwnerMessage(message, ownerId, botId)) emit(toInbound(message));
    store.save(message.id); // Checkpoint only after successful delivery; filtered messages still advance the cursor.
  };
  const pollChannel = async (
    channelId: string,
    store: DiscordInboundStore,
    emit: (message: DiscordInbound) => void,
  ): Promise<void> => {
    const after = store.load();
    if (after === undefined) {
      throw new MessengerError("Discord inbound cursor disappeared; restore its state before retrying", 0);
    }
    const pending = new Map<string, Message>();
    let before: string | undefined;
    for (;;) {
      const messages = await readPage(channelId, 100, before);
      if (messages === undefined) return;
      let oldest: string | undefined;
      for (const message of messages) {
        if (oldest === undefined || newer(oldest, message.id)) oldest = message.id;
        if (message.channel_id === channelId && newer(message.id, after) && !pending.has(message.id)) {
          pending.set(message.id, message);
        }
      }
      if (before !== undefined && oldest !== undefined && !newer(before, oldest)) {
        throw inboundProtocolError("Discord inbound pagination did not advance");
      }
      if (messages.length < 100 || oldest === undefined || !newer(oldest, after)) break;
      before = oldest;
    }
    const ordered = [...pending.values()].sort((a, b) => (newer(a.id, b.id) ? 1 : newer(b.id, a.id) ? -1 : 0));
    if (gate) await gate.admit(channelId, store, ordered, emit);
    else for (const message of ordered) deliver(message, store, emit);
  };
  await refresh(true);
  let polls = 0;
  return {
    botId,
    channels: (): string[] => [...watched.keys()],
    /** Reads every watched channel from its cursor; the REST path and the Gateway backfill. */
    async poll(emit: (message: DiscordInbound) => void): Promise<void> {
      // Pick up channels and threads created since start: scope all every 100 polls, followed threads every 10.
      if (opts.all ? ++polls % 100 === 0 : threads && ++polls % 10 === 0) await refresh(false);
      for (const [channelId, store] of [...watched]) await pollChannel(channelId, store, emit);
    },
    /** Reconciles the watched set, then reads each channel from its cursor; used after a Gateway (re)connect. */
    async sync(emit: (message: DiscordInbound) => void): Promise<void> {
      await refresh(false);
      await this.poll(emit);
    },
    /** A MESSAGE_CREATE dispatch: delivered only when newer than its channel's cursor. */
    async accept(message: Message, emit: (message: DiscordInbound) => void): Promise<void> {
      if (!message || !isDiscordSnowflake(message.id) || !isDiscordSnowflake(message.channel_id)) return;
      let store = watched.get(message.channel_id);
      if (!store) {
        const parent = threads ? await following.parentOf(message.channel_id) : undefined;
        // Scope `all` takes any readable channel; otherwise only a thread under a watched channel is new to us.
        if (!opts.all && parent === undefined) return;
        store = storeFor(message.channel_id);
        const seen = store.load();
        if (seen === undefined) {
          // Scope all starts just before this message; a new thread starts at the parent's floor, never earlier.
          const seed =
            parent === undefined ? String(BigInt(message.id) - 1n) : following.adopt(message.channel_id, parent);
          store.save(seed);
        }
        watched.set(message.channel_id, store);
      }
      const cursor = store.load();
      if (cursor === undefined) {
        throw new MessengerError("Discord inbound cursor disappeared; restore its state before retrying", 0);
      }
      if (!newer(message.id, cursor)) return;
      if (gate) await gate.admit(message.channel_id, store, [message], emit);
      else deliver(message, store, emit);
    },
    /** A MESSAGE_UPDATE dispatch; only mention routing reads edits. */
    async update(message: Message, emit: (message: DiscordInbound) => void): Promise<void> {
      if (!gate || !message || !isDiscordSnowflake(message.id) || !isDiscordSnowflake(message.channel_id)) return;
      const store = watched.get(message.channel_id);
      if (store) await gate.update(message, store, emit);
    },
    /** Routes messages held for a burst decision once it may be final; see `wakeIn`. */
    async settle(emit: (message: DiscordInbound) => void): Promise<void> {
      for (const id of gate?.held() ?? []) await gate?.admit(id, watched.get(id) ?? storeFor(id), [], emit);
    },
    /** Milliseconds until the earliest held burst decision is final, on the routing clock; undefined when none is held. */
    wakeIn(): number | undefined {
      const at = gate?.wakeAt();
      return at === undefined || !routing ? undefined : Math.max(0, at - routing.now());
    },
  };
};
