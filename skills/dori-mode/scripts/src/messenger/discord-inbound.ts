import type { Discord } from "./discord.ts";
import { discordInboundEnv, type DiscordInboundStore, isDiscordSnowflake } from "./discord-inbound-state.ts";
import { MessengerError } from "./http.ts";

type Message = {
  id: string;
  channel_id: string;
  type?: number;
  author?: { id?: string; bot?: boolean; system?: boolean };
  webhook_id?: string;
  content?: string;
  message_reference?: { message_id?: string };
};
export type DiscordInbound = { channel: string; message: string; reference: string | null; text: string };
export type DiscordInboundOptions = { ownerId: string; channelId: string };

export const formatDiscordInbound = (message: DiscordInbound): string =>
  `INBOUND discord ${message.channel} ${message.message} from=owner ref=${message.reference ?? "-"} ${JSON.stringify(message.text)}`;

/** A bot-authenticated, exact-channel REST intake. Presence remains a separate Gateway connection. */
export const discordInbound = async (discord: Discord, store: DiscordInboundStore, opts: DiscordInboundOptions) => {
  const ownerId = discordInboundEnv("DORI_DISCORD_OWNER_ID", opts.ownerId);
  const channelId = discordInboundEnv("DORI_DISCORD_CHANNEL_ID", opts.channelId);
  const request = async <T>(path: string): Promise<T> => {
    try {
      return await discord.call<T>("GET", path);
    } catch (error) {
      const status = error instanceof MessengerError ? error.status : 0;
      let message = "Discord inbound request failed; check the connection and Discord API availability";
      if (status === 401)
        message = "Discord inbound authentication failed; set DORI_DISCORD_TOKEN to a valid bot token";
      else if (status === 403) {
        message =
          "Discord inbound access denied; grant View Channel, Read Message History and access to the configured thread";
      } else if (status === 404) {
        message = "Discord inbound channel is unavailable; check DORI_DISCORD_CHANNEL_ID and the bot's channel access";
      } else if (status === 429)
        message = "Discord inbound remains rate limited after retries; reduce the polling frequency";
      throw new MessengerError(message, status);
    }
  };
  const me = await request<{ id?: string; bot?: boolean }>("/users/@me");
  if (!me || me.bot !== true || !isDiscordSnowflake(me.id)) {
    throw new MessengerError("Discord inbound requires an authenticated bot identity; use a bot token", 401);
  }
  if (me.id === ownerId)
    throw new MessengerError("DORI_DISCORD_OWNER_ID must identify the human owner, not the bot", 0);
  const application = await request<{ flags?: number }>("/applications/@me");
  const flags = application?.flags;
  if (typeof flags !== "number" || !Number.isSafeInteger(flags) || (flags & ((1 << 18) | (1 << 19))) === 0) {
    throw new MessengerError(
      "Discord inbound needs Message Content Intent enabled for this bot in the Developer Portal",
      403,
    );
  }
  const page = async (limit: number, before?: string): Promise<readonly Message[]> => {
    const value = await request<unknown>(
      `/channels/${channelId}/messages?limit=${limit}${before === undefined ? "" : `&before=${before}`}`,
    );
    if (!Array.isArray(value) || value.length > limit) {
      throw new MessengerError("Discord inbound message response is invalid", 502);
    }
    const messages = value as Message[];
    for (const message of messages) {
      if (!message || typeof message !== "object" || !isDiscordSnowflake(message.id)) {
        throw new MessengerError("Discord inbound message response is invalid", 502);
      }
    }
    return messages;
  };
  let cursor = store.load();
  const latest = await page(1); // Also verify channel/history access before announcing readiness on a restart.
  if (latest.some((message) => message.channel_id !== channelId)) {
    throw new MessengerError("Discord inbound channel response does not match DORI_DISCORD_CHANNEL_ID", 502);
  }
  if (cursor === undefined) {
    cursor = latest[0]?.id ?? "0";
    store.save(cursor); // Cold start intentionally skips existing history, including the latest owner message.
  }
  return {
    botId: me.id,
    async poll(emit: (message: DiscordInbound) => void): Promise<void> {
      const after = store.load();
      if (after === undefined) {
        throw new MessengerError("Discord inbound cursor disappeared; restore its state before retrying", 0);
      }
      const pending = new Map<string, Message>();
      let before: string | undefined;
      for (;;) {
        const messages = await page(100, before);
        let oldest: string | undefined;
        for (const message of messages) {
          if (oldest === undefined || BigInt(message.id) < BigInt(oldest)) oldest = message.id;
          if (message.channel_id === channelId && BigInt(message.id) > BigInt(after) && !pending.has(message.id)) {
            pending.set(message.id, message);
          }
        }
        if (before !== undefined && oldest !== undefined && BigInt(oldest) >= BigInt(before)) {
          throw new MessengerError("Discord inbound pagination did not advance", 502);
        }
        if (messages.length < 100 || oldest === undefined || BigInt(oldest) <= BigInt(after)) break;
        before = oldest;
      }
      const ordered = [...pending.values()].sort((a, b) =>
        BigInt(a.id) < BigInt(b.id) ? -1 : BigInt(a.id) > BigInt(b.id) ? 1 : 0,
      );
      for (const message of ordered) {
        if (
          message.author?.id === ownerId &&
          message.author.id !== me.id &&
          !message.author.bot &&
          !message.author.system &&
          !message.webhook_id &&
          (message.type === 0 || message.type === 19)
        ) {
          if (typeof message.content !== "string") {
            throw new MessengerError(
              "Discord inbound owner message content is unavailable; enable Message Content Intent",
              403,
            );
          }
          const reference = message.message_reference?.message_id;
          emit({
            channel: channelId,
            message: message.id,
            reference: isDiscordSnowflake(reference) ? reference : null,
            text: message.content,
          });
        }
        store.save(message.id); // Checkpoint only after successful delivery; filtered messages still advance the cursor.
      }
    },
  };
};
