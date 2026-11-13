import { type DcCursor, type DcStore, snowflakeAfter, snowflakeAt } from "./discord-intake-cursor.ts";
import { type DcChannel, type DcMessage, type Discord, isThreadType } from "./discord.ts";

export type DcFile = { readonly kind: string; readonly url: string };

export type DcInbound = {
  readonly kind: "message" | "edit";
  readonly source: "dm" | "channel" | "thread";
  /** The DM channel, or the text/forum channel (a thread's parent). */
  readonly channel: string;
  readonly thread?: string;
  /** The message id for a new message; for an edit, a snowflake of the edit time, so every edit line differs. */
  readonly eventId: string;
  readonly messageId: string;
  readonly from: string;
  readonly text: string;
  readonly files: readonly DcFile[];
};

/**
 * Owner-gated Discord inbound: the owner's DMs to the bot, and the owner's messages in the bot's guilds — only
 * `guildId` when set, and only `channelIds` (and their threads and forum posts) when set. Bots never pass.
 */
export type DcInboundOptions = {
  readonly ownerId: string;
  readonly guildId?: string;
  readonly channelIds?: readonly string[];
};

/** The INBOUND line, in `dori inbound telegram`'s field order; one ` file=` per attachment. */
export const formatDiscordIntake = (i: DcInbound): string =>
  `INBOUND ${i.kind === "edit" ? "edit:" : ""}${i.source} ${i.channel} ${i.thread ?? "-"} ${i.eventId} mid=${i.messageId} ${i.from} ${JSON.stringify(i.text)}` +
  i.files.map((f) => ` file=${f.kind}:${f.url}`).join("");

const IS_VOICE_MESSAGE = 1 << 13;
const DM = 1;
/** Channels whose messages catch-up reads: text and announcement channels (threads come from the thread list). */
const TEXT_TYPES = new Set([0, 5]);
/** Catch-up re-reads this much before the watermark: gateway delivery lag and clock skew. */
const MARGIN_MS = 60_000;

const filesOf = (m: DcMessage): DcFile[] =>
  (m.attachments ?? []).map((a) => {
    const type = a.content_type ?? "";
    const kind =
      ((m.flags ?? 0) & IS_VOICE_MESSAGE) !== 0 || a.waveform !== undefined
        ? "voice"
        : type.startsWith("audio/")
          ? "audio"
          : type.startsWith("video/")
            ? "video"
            : type.startsWith("image/")
              ? "photo"
              : "document";
    return { kind, url: a.url };
  });

export class DiscordIntake {
  private readonly cursor: DcCursor;
  private readonly places = new Map<string, DcChannel>();
  private readonly edits = new Set<string>();

  constructor(
    private readonly dc: Discord,
    private readonly store: DcStore,
    private readonly opts: DcInboundOptions,
    private readonly emit: (i: DcInbound) => void,
    private readonly warn: (line: string) => void,
    private readonly now: () => number,
  ) {
    this.cursor = store.load();
  }

  /** Remember a channel's type and parent from a gateway event or REST answer. */
  learn(ch: DcChannel, guildId?: string): void {
    this.places.set(ch.id, guildId === undefined || ch.guild_id !== undefined ? ch : { ...ch, guild_id: guildId });
  }

  /** Gateway dispatches that describe channels; everything else is ignored here. */
  learnFrom(type: string, d: unknown): void {
    const g = d as { id?: string; guild_id?: string; channels?: DcChannel[]; threads?: DcChannel[] };
    if (type === "GUILD_CREATE") for (const ch of [...(g.channels ?? []), ...(g.threads ?? [])]) this.learn(ch, g.id);
    else if (type === "THREAD_LIST_SYNC") for (const ch of g.threads ?? []) this.learn(ch, g.guild_id);
    else if (/^(CHANNEL|THREAD)_(CREATE|UPDATE)$/.test(type)) this.learn(d as DcChannel);
  }

  private async place(m: DcMessage): Promise<DcChannel | undefined> {
    const known = this.places.get(m.channel_id);
    if (known) return known;
    if (m.guild_id === undefined) {
      const dm = { id: m.channel_id, type: DM };
      this.learn(dm);
      return dm;
    }
    try {
      const ch = await this.dc.channel(m.channel_id);
      this.learn(ch, m.guild_id);
      return this.places.get(ch.id);
    } catch (e) {
      this.warn(`discord channel ${m.channel_id}: ${e instanceof Error ? e.message : String(e)}`);
      return undefined;
    }
  }

  private passes(m: DcMessage, ch: DcChannel): boolean {
    if (m.author?.id !== this.opts.ownerId || m.author.bot === true || m.webhook_id !== undefined) return false;
    if (m.type !== undefined && m.type !== 0 && m.type !== 19) return false; // plain messages and replies only
    if (ch.type === DM) return true;
    const guild = m.guild_id ?? ch.guild_id;
    if (this.opts.guildId !== undefined && guild !== this.opts.guildId) return false;
    if (this.opts.channelIds === undefined) return true;
    const top = isThreadType(ch.type) ? ch.parent_id : ch.id;
    return typeof top === "string" && this.opts.channelIds.includes(top);
  }

  /**
   * A new message (live or caught up). Delivery is at-least-once: the channel's cursor moves past it only after
   * `emit` returned, and anything at or before the cursor (a resume replay, a catch-up overlap) is skipped.
   */
  async message(m: DcMessage, known?: DcChannel): Promise<void> {
    const seen = this.cursor.channels[m.channel_id];
    if (seen !== undefined && !snowflakeAfter(m.id, seen)) return;
    const ch = known ?? (await this.place(m));
    if (ch === undefined) return;
    if (this.passes(m, ch)) this.emit(this.inbound(m, ch, "message", m.id));
    this.cursor.channels[m.channel_id] = m.id;
    this.store.save(this.cursor);
  }

  /** An edit: only a real content edit (edited_timestamp set) passes, once per edit; embed unfurls do not. */
  async edit(m: DcMessage): Promise<void> {
    if (!m.edited_timestamp || m.content === undefined || m.author === undefined) return;
    const key = `${m.id}:${m.edited_timestamp}`;
    if (this.edits.has(key)) return;
    const ch = await this.place(m);
    if (ch === undefined || !this.passes(m, ch)) return;
    this.emit(this.inbound(m, ch, "edit", snowflakeAt(Date.parse(m.edited_timestamp))));
    if (this.edits.size > 1_000) this.edits.clear();
    this.edits.add(key);
  }

  private inbound(m: DcMessage, ch: DcChannel, kind: DcInbound["kind"], eventId: string): DcInbound {
    const thread = isThreadType(ch.type) ? ch.id : undefined;
    return {
      kind,
      source: ch.type === DM ? "dm" : thread !== undefined ? "thread" : "channel",
      channel: thread !== undefined && ch.parent_id ? ch.parent_id : ch.id,
      ...(thread === undefined ? {} : { thread }),
      eventId,
      messageId: m.id,
      from: m.author?.id ?? "-",
      text: m.content ?? "",
      files: filesOf(m),
    };
  }

  /** Everything up to now is processed (the live session is acked and the queue is drained). */
  mark(): void {
    const upTo = snowflakeAt(this.now() - MARGIN_MS);
    if (this.cursor.upTo !== undefined && !snowflakeAfter(upTo, this.cursor.upTo)) return;
    this.cursor.upTo = upTo;
    this.store.save(this.cursor);
  }

  private async visible(): Promise<DcChannel[]> {
    const out: DcChannel[] = [{ ...(await this.dc.openDm(this.opts.ownerId)), type: DM }];
    const guilds = this.opts.guildId === undefined ? (await this.dc.guilds()).map((g) => g.id) : [this.opts.guildId];
    const allowed = (id: string | null | undefined) =>
      this.opts.channelIds === undefined || (typeof id === "string" && this.opts.channelIds.includes(id));
    for (const guild of guilds) {
      for (const ch of await this.dc.guildChannels(guild))
        if (TEXT_TYPES.has(ch.type) && allowed(ch.id)) out.push({ ...ch, guild_id: guild });
        else this.learn(ch, guild);
      for (const th of await this.dc.activeThreads(guild))
        if (allowed(th.parent_id)) out.push({ ...th, guild_id: guild });
        else this.learn(th, guild);
    }
    for (const ch of out) this.learn(ch);
    return out;
  }

  /**
   * Read every visible channel's messages since the stored cursor over REST — after a fresh gateway session, in
   * one-shot mode, or while the gateway is down. The first run only records where to start: no history replay.
   * Returns false when some channel could not be read; the watermark then stays where it was.
   */
  async catchUp(): Promise<boolean> {
    const startedAt = this.now();
    const { upTo, channels } = this.cursor;
    if (upTo === undefined && Object.keys(channels).length === 0) {
      this.mark();
      return true;
    }
    let list: DcChannel[];
    try {
      list = await this.visible();
    } catch (e) {
      this.warn(`discord catch-up: ${e instanceof Error ? e.message : String(e)}`);
      return false;
    }
    let complete = true;
    for (const ch of list) {
      const own = channels[ch.id];
      let after = own === undefined ? upTo : upTo === undefined || snowflakeAfter(own, upTo) ? own : upTo;
      if (after === undefined) after = snowflakeAt(startedAt - MARGIN_MS);
      // null: the channel never had a message; an id at or before the cursor: nothing new.
      if (ch.last_message_id === null || (ch.last_message_id && !snowflakeAfter(ch.last_message_id, after))) continue;
      for (;;) {
        let page: readonly DcMessage[];
        try {
          page = await this.dc.messagesAfter(ch.id, after);
        } catch (e) {
          this.warn(`discord catch-up ${ch.id}: ${e instanceof Error ? e.message : String(e)}`);
          complete = false;
          break;
        }
        const ordered = [...page].sort((a, b) => (snowflakeAfter(a.id, b.id) ? 1 : -1));
        for (const m of ordered) await this.message({ ...m, channel_id: ch.id }, ch);
        if (page.length < 100 || ordered.length === 0) break;
        after = ordered.at(-1)!.id;
      }
    }
    if (complete) {
      const upToNow = snowflakeAt(startedAt - MARGIN_MS);
      if (this.cursor.upTo === undefined || snowflakeAfter(upToNow, this.cursor.upTo)) this.cursor.upTo = upToNow;
      this.store.save(this.cursor);
    }
    return complete;
  }
}
