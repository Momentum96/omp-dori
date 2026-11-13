import type { Clock } from "../run.ts";
import { guardText, type Http, MessengerError, withBackoff } from "./http.ts";

const retryAfter = (body: string): number | undefined => {
  try {
    const n = (JSON.parse(body) as { retry_after?: number }).retry_after;
    return typeof n === "number" ? n : undefined;
  } catch {
    return undefined;
  }
};

export type DcAttachment = {
  readonly url: string;
  readonly content_type?: string;
  /** Present on voice messages (with the message flag IS_VOICE_MESSAGE). */
  readonly waveform?: string;
};
export type DcMessage = {
  readonly id: string;
  readonly channel_id: string;
  /** Gateway events carry it; REST message lists do not. */
  readonly guild_id?: string;
  readonly type?: number;
  readonly author?: { readonly id: string; readonly bot?: boolean };
  readonly webhook_id?: string;
  readonly content?: string;
  readonly edited_timestamp?: string | null;
  readonly flags?: number;
  readonly attachments?: readonly DcAttachment[];
};
export type DcTag = { readonly id: string; readonly name: string };
export type DcChannel = {
  readonly id: string;
  readonly type: number;
  readonly name?: string;
  readonly guild_id?: string;
  readonly parent_id?: string | null;
  readonly last_message_id?: string | null;
  readonly available_tags?: readonly DcTag[];
};

/** Channel types: 10-12 are threads; 15 (forum) and 16 (media) hold posts, which are threads. */
export const isThreadType = (type: number): boolean => type === 10 || type === 11 || type === 12;
export const isForumType = (type: number): boolean => type === 15 || type === 16;

export class Discord {
  constructor(
    private readonly http: Http,
    private readonly clock: Clock,
    private readonly botToken: string,
    private readonly base = "https://discord.com/api/v10",
  ) {}

  async call<T = unknown>(
    method: "GET" | "POST" | "PATCH" | "PUT" | "DELETE",
    path: string,
    body?: Record<string, unknown>,
  ): Promise<T> {
    const res = await withBackoff(
      this.http,
      this.clock,
      {
        method,
        url: `${this.base}${path}`,
        headers: { Authorization: `Bot ${this.botToken}`, "Content-Type": "application/json" },
        ...(body ? { body: JSON.stringify(body) } : {}),
      },
      undefined,
      retryAfter,
    );
    if (res.status >= 300)
      throw new MessengerError(`discord ${method} ${path}: ${res.status} ${res.body.slice(0, 160)}`, res.status);
    return (res.body ? JSON.parse(res.body) : {}) as T;
  }

  async send(channelId: string, content: string): Promise<string> {
    return (
      await this.call<{ id: string }>("POST", `/channels/${channelId}/messages`, {
        content: guardText(content),
        allowed_mentions: { parse: [] },
      })
    ).id;
  }

  async edit(channelId: string, messageId: string, content: string): Promise<void> {
    await this.call("PATCH", `/channels/${channelId}/messages/${messageId}`, { content: guardText(content) });
  }

  async typing(channelId: string): Promise<void> {
    await this.call("POST", `/channels/${channelId}/typing`);
  }

  async startThread(channelId: string, messageId: string, name: string): Promise<string> {
    return (
      await this.call<{ id: string }>("POST", `/channels/${channelId}/messages/${messageId}/threads`, {
        name: guardText(name).slice(0, 100),
      })
    ).id;
  }

  async setThread(
    threadId: string,
    patch: { readonly name?: string; readonly archived?: boolean; readonly appliedTags?: readonly string[] },
  ): Promise<void> {
    await this.call("PATCH", `/channels/${threadId}`, {
      ...(patch.name ? { name: guardText(patch.name).slice(0, 100) } : {}),
      ...(patch.archived !== undefined ? { archived: patch.archived } : {}),
      ...(patch.appliedTags !== undefined ? { applied_tags: patch.appliedTags } : {}),
    });
  }

  /**
   * A new thread without a starter message. In a forum the post's first message is required, so `text` (or the
   * name) becomes it and its id equals the thread id; in a text channel `text`, when given, is posted inside.
   */
  async createThread(
    channel: DcChannel,
    name: string,
    opts: { readonly text?: string; readonly appliedTags?: readonly string[] } = {},
  ): Promise<{ readonly id: string; readonly messageId?: string }> {
    const title = guardText(name).slice(0, 100);
    if (isForumType(channel.type)) {
      const t = await this.call<{ id: string }>("POST", `/channels/${channel.id}/threads`, {
        name: title,
        message: { content: guardText(opts.text ?? name), allowed_mentions: { parse: [] } },
        ...(opts.appliedTags?.length ? { applied_tags: opts.appliedTags } : {}),
      });
      return { id: t.id, messageId: t.id };
    }
    const t = await this.call<{ id: string }>("POST", `/channels/${channel.id}/threads`, { name: title, type: 11 });
    return opts.text === undefined ? { id: t.id } : { id: t.id, messageId: await this.send(t.id, opts.text) };
  }

  /** Add (or with `remove`, take back) the bot's own reaction; the read receipt is 👀. */
  async react(channelId: string, messageId: string, emoji: string, remove = false): Promise<void> {
    await this.call(
      remove ? "DELETE" : "PUT",
      `/channels/${channelId}/messages/${messageId}/reactions/${encodeURIComponent(emoji)}/@me`,
    );
  }

  channel(id: string): Promise<DcChannel> {
    return this.call<DcChannel>("GET", `/channels/${id}`);
  }

  /** Up to `limit` messages right after `after`; Discord lists them newest first. */
  messagesAfter(channelId: string, after: string, limit = 100): Promise<readonly DcMessage[]> {
    return this.call<readonly DcMessage[]>("GET", `/channels/${channelId}/messages?after=${after}&limit=${limit}`);
  }

  me(): Promise<{ readonly id: string }> {
    return this.call<{ id: string }>("GET", "/users/@me");
  }

  guilds(): Promise<readonly { readonly id: string }[]> {
    return this.call<readonly { id: string }[]>("GET", "/users/@me/guilds");
  }

  guildChannels(guildId: string): Promise<readonly DcChannel[]> {
    return this.call<readonly DcChannel[]>("GET", `/guilds/${guildId}/channels`);
  }

  async activeThreads(guildId: string): Promise<readonly DcChannel[]> {
    return (
      (await this.call<{ threads?: readonly DcChannel[] }>("GET", `/guilds/${guildId}/threads/active`)).threads ?? []
    );
  }

  /** The bot's DM channel with a user (Discord returns the existing one). */
  openDm(userId: string): Promise<DcChannel> {
    return this.call<DcChannel>("POST", "/users/@me/channels", { recipient_id: userId });
  }
}
