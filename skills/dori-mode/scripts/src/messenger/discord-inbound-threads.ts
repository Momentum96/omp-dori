import { channelInfo, inboundProtocolError, newer, type Request, unavailable } from "./discord-inbound-api.ts";
import { type DiscordInboundStore, isDiscordSnowflake } from "./discord-inbound-state.ts";
import { MessengerError } from "./http.ts";

/** Everything thread following shares with the channel intake that owns it. */
export type ThreadWatch = {
  request: Request;
  /** Configured channel and thread IDs: never dropped, and their failures stay fatal. */
  explicit: readonly string[];
  watched: Map<string, DiscordInboundStore>;
  /** Watched plain channels with the position captured when each was first attached. */
  parents: Map<string, string>;
  /** Watches one channel; `seed` sets a cold-start cursor instead of the channel's latest message. */
  attach: (channelId: string, seed?: string) => Promise<string | undefined>;
};

/** Every active thread in one guild with its parent channel; `undefined` when that guild is inaccessible. */
const activeThreads = async (
  request: Request,
  guildId: string,
): Promise<{ id: string; parent: string }[] | undefined> => {
  let value: unknown;
  try {
    value = await request<unknown>(`/guilds/${guildId}/threads/active`);
  } catch (error) {
    if (error instanceof MessengerError && (error.status === 403 || error.status === 404)) return undefined;
    throw error;
  }
  const threads: unknown = (value as { threads?: unknown } | null)?.threads;
  if (!Array.isArray(threads)) throw inboundProtocolError("Discord inbound thread response is invalid");
  const found: { id: string; parent: string }[] = [];
  for (const thread of threads) {
    if (!thread || typeof thread !== "object" || Array.isArray(thread)) {
      throw inboundProtocolError("Discord inbound thread response is invalid");
    }
    const { id, parent_id } = thread as Record<string, unknown>;
    if (isDiscordSnowflake(id) && isDiscordSnowflake(parent_id)) found.push({ id, parent: parent_id });
  }
  return found;
};

/**
 * Follows active threads whose parent is a watched channel: lists them per guild, keeps one cursor per thread
 * through the intake's stores, and drops archived threads while keeping their cursors for a later reactivation.
 */
export const discordThreads = (watch: ThreadWatch) => {
  const { request, explicit, watched, parents, attach } = watch;
  /** Cached thread parent per channel id; `null` means the channel is not a thread or is inaccessible. */
  const threadParent = new Map<string, string | null>();
  /** Threads watched only because a listing or a live dispatch named them. */
  const discovered = new Set<string>();
  /** Active threads whose history read was denied; retried only after the listing drops them. */
  const skipped = new Set<string>();
  const guildOf = new Map<string, string | null>();
  const guildFor = async (channelId: string): Promise<string | undefined> => {
    const cached = guildOf.get(channelId);
    if (cached !== undefined) return cached ?? undefined;
    const guild = (await channelInfo(request, channelId, false))?.guild ?? null;
    guildOf.set(channelId, guild);
    return guild ?? undefined;
  };
  /** A thread first seen after startup starts at its own creation, never before the parent's start position. */
  const seedFor = (threadId: string, parent: string): string => {
    const floor = parents.get(parent);
    return floor === undefined || newer(threadId, floor) ? threadId : floor;
  };
  const follow = async (initial: boolean): Promise<void> => {
    if (parents.size === 0) return;
    const guilds = new Set<string>();
    for (const parent of parents.keys()) {
      if (!watched.has(parent)) continue;
      const guild = await guildFor(parent);
      if (guild !== undefined) guilds.add(guild);
    }
    const active = new Map<string, string>();
    for (const guild of guilds) {
      for (const thread of (await activeThreads(request, guild)) ?? []) {
        if (parents.has(thread.parent)) active.set(thread.id, thread.parent);
      }
    }
    for (const id of [...discovered]) {
      if (active.has(id)) continue;
      discovered.delete(id);
      skipped.delete(id);
      threadParent.delete(id);
      if (!explicit.includes(id)) watched.delete(id); // Archived thread: keep its cursor, stop polling it.
    }
    for (const [id, parent] of active) {
      if (skipped.has(id)) continue;
      threadParent.set(id, parent);
      if (watched.has(id)) {
        discovered.add(id);
        continue;
      }
      try {
        // The startup listing baselines like any cold start; a thread found later starts at its first message.
        if ((await attach(id, initial ? undefined : seedFor(id, parent))) === undefined) {
          skipped.add(id);
          continue;
        }
        discovered.add(id);
      } catch (error) {
        if (explicit.includes(id) || !unavailable(error)) throw error;
        skipped.add(id); // Denied history: an operator must grant access; do not retry every refresh.
      }
    }
  };
  /** Records an attached configured channel: a plain parent, or a thread that already watches itself. */
  const register = async (channelId: string, latest: string): Promise<void> => {
    const info = await channelInfo(request, channelId, false);
    guildOf.set(channelId, info?.guild ?? null); // Cached: the listing and live lookups reuse it.
    if (info?.parent !== undefined) threadParent.set(channelId, info.parent);
    else parents.set(channelId, latest);
  };
  /** The watched parent of a live channel; `undefined` when it is not a thread under a watched channel. */
  const parentOf = async (channelId: string): Promise<string | undefined> => {
    const cached = threadParent.get(channelId);
    if (cached !== undefined) return cached ?? undefined;
    if (parents.size === 0) return undefined; // No watched plain channel can own a thread: skip the lookup.
    const info = await channelInfo(request, channelId, true);
    const parent = info?.parent !== undefined && parents.has(info.parent) ? info.parent : null;
    threadParent.set(channelId, parent);
    return parent ?? undefined;
  };
  /** Adopts a thread first seen through a live dispatch, so it is polled and never dropped as unlisted. */
  const adopt = (threadId: string, parent: string): string => {
    discovered.add(threadId);
    threadParent.set(threadId, parent);
    return seedFor(threadId, parent);
  };
  return { follow, register, parentOf, adopt };
};
