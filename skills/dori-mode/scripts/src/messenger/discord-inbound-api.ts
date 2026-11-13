import type { Discord } from "./discord.ts";
import { isDiscordSnowflake } from "./discord-inbound-state.ts";
import { MessengerError } from "./http.ts";

export type Message = {
  id: string;
  channel_id: string;
  type?: number;
  author?: { id?: string; bot?: boolean; system?: boolean };
  webhook_id?: string;
  content?: string;
  message_reference?: { message_id?: string };
};
export type DiscordInbound = { channel: string; message: string; reference: string | null; text: string };
export type Request = <T>(path: string, body?: Record<string, unknown>) => Promise<T>;

const REQUEST_FAILED = "Discord inbound request failed; check the connection and Discord API availability";

/** A rate limit, a server error or an unreachable API: worth retrying; anything else needs the operator. */
export const isTransient = (error: unknown): boolean =>
  error instanceof MessengerError && (error.status === 429 || error.status >= 500 || error.message === REQUEST_FAILED);

/** Maps Discord failures to actionable errors that never echo a response body. */
export const inboundRequest =
  (discord: Discord): Request =>
  async <T>(path: string, body?: Record<string, unknown>): Promise<T> => {
    try {
      return await discord.call<T>(body ? "POST" : "GET", path, body);
    } catch (error) {
      const status = error instanceof MessengerError ? error.status : 0;
      let message = REQUEST_FAILED;
      if (status === 401)
        message = "Discord inbound authentication failed; set DORI_DISCORD_TOKEN to a valid bot token";
      else if (status === 403) {
        message =
          "Discord inbound access denied; grant View Channel, Read Message History and access to the configured thread";
      } else if (status === 404) {
        message =
          "Discord inbound channel is unavailable; check DORI_DISCORD_CHANNEL_ID, DORI_DISCORD_CHANNEL_IDS and the bot's channel access";
      } else if (status === 429)
        message = "Discord inbound remains rate limited after retries; reduce the polling frequency";
      throw new MessengerError(message, status);
    }
  };

export const messagePage = async (
  request: Request,
  channelId: string,
  limit: number,
  before?: string,
): Promise<readonly Message[]> => {
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

/** Plain owner text or a reply: not the bot itself, another bot, a webhook, or a system message. */
export const isOwnerText = (message: Message, ownerId: string, botId: string): boolean =>
  message.author?.id === ownerId &&
  message.author.id !== botId &&
  !message.author.bot &&
  !message.author.system &&
  !message.webhook_id &&
  (message.type === 0 || message.type === 19);

export const toInbound = (message: Message): DiscordInbound => {
  if (typeof message.content !== "string") {
    throw new MessengerError(
      "Discord inbound owner message content is unavailable; enable Message Content Intent",
      403,
    );
  }
  const reference = message.message_reference?.message_id;
  return {
    channel: message.channel_id,
    message: message.id,
    reference: isDiscordSnowflake(reference) ? reference : null,
    text: message.content,
  };
};

const TEXT_CHANNELS = new Set([0, 5]); // text and announcement

/** Every text channel and active thread in the bot's guilds, plus the owner's DM with the bot. */
export const discoverChannels = async (request: Request, ownerId: string): Promise<string[]> => {
  const ids = new Set<string>();
  const guilds = await request<{ id?: string }[]>("/users/@me/guilds");
  for (const guild of Array.isArray(guilds) ? guilds : []) {
    if (!isDiscordSnowflake(guild?.id)) continue;
    const channels = await request<{ id?: string; type?: number }[]>(`/guilds/${guild.id}/channels`);
    for (const channel of Array.isArray(channels) ? channels : []) {
      if (isDiscordSnowflake(channel?.id) && TEXT_CHANNELS.has(channel.type ?? -1)) ids.add(channel.id);
    }
    const threads = await request<{ threads?: { id?: string }[] }>(`/guilds/${guild.id}/threads/active`);
    for (const thread of Array.isArray(threads?.threads) ? threads.threads : []) {
      if (isDiscordSnowflake(thread?.id)) ids.add(thread.id);
    }
  }
  const dm = await request<{ id?: string }>("/users/@me/channels", { recipient_id: ownerId });
  if (isDiscordSnowflake(dm?.id)) ids.add(dm.id);
  return [...ids];
};
