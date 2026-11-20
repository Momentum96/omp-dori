import {
  channelInfo,
  type DiscordInbound,
  inboundProtocolError,
  type Message,
  newer,
  type Request,
  toInbound,
  unavailable,
} from "./discord-inbound-api.ts";
import { type DiscordInboundStore, isDiscordSnowflake } from "./discord-inbound-state.ts";
import {
  type Bot,
  BOTS,
  type Neighbor,
  type Reason,
  type RouteConfig,
  type RouteResult,
  route,
  type ThreadInfo,
} from "./discord-route.ts";
import { MessengerError } from "./http.ts";

/** This receiver's bot, the shared topology and the clock burst decisions read. */
export type DiscordRouting = { readonly bot: Bot; readonly config: RouteConfig; readonly now: () => number };
/** Saved with a channel's cursor: the owner-thread assignee and the latest eligible routing decisions. */
export type RouteState = { readonly assignee: Bot | null; readonly recent: readonly Neighbor[] };
type Emit = (message: DiscordInbound) => void;

const RECENT_LIMIT = 50;
const REASONS: readonly Reason[] = [
  "ineligible-webhook",
  "ineligible-type",
  "ineligible-author",
  "ineligible-channel",
  "explicit",
  "reply",
  "thread-owner",
  "thread-starter",
  "thread-assignee",
  "burst",
  "burst-pending",
  "unaddressed",
];
const isBot = (value: unknown): value is Bot => BOTS.some((bot) => bot === value);
const isReason = (value: unknown): value is Reason => REASONS.some((reason) => reason === value);
const invalidState = () =>
  new MessengerError("Discord inbound routing state is invalid; repair it before restarting intake", 0);
const byId = (a: Message, b: Message): number => (newer(a.id, b.id) ? 1 : newer(b.id, a.id) ? -1 : 0);
/** Only an explicit mention or a reply moves an owner thread's assignee. */
const assigns = (result: RouteResult): boolean =>
  result.assignee !== undefined && (result.reason === "explicit" || result.reason === "reply");

export const parseRouteState = (raw: unknown): RouteState => {
  if (raw === undefined) return { assignee: null, recent: [] };
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) throw invalidState();
  const { assignee, recent } = raw as Record<string, unknown>;
  if ((assignee !== null && !isBot(assignee)) || !Array.isArray(recent)) throw invalidState();
  return {
    assignee,
    recent: recent.map((entry: unknown) => {
      if (!entry || typeof entry !== "object") throw invalidState();
      const { id, addressed, reason } = entry as Record<string, unknown>;
      if (!isDiscordSnowflake(id) || !Array.isArray(addressed) || !addressed.every(isBot) || !isReason(reason)) {
        throw invalidState();
      }
      return { id, addressed, reason };
    }),
  };
};

/**
 * Mention routing for one bot's intake (the shared `route()` rules). Messages above a channel's cursor wait in a
 * per-channel buffer until routed; each decided message is emitted only when it addresses this bot, then checkpointed
 * with the routing state, so dropped messages still advance the cursor. A message whose burst decision is still open
 * holds itself and every later message of its channel until `wakeAt`, keeping each channel in ascending order.
 */
export const discordRouting = (routing: DiscordRouting, request: Request) => {
  const { bot, config, now } = routing;
  const homes = new Set([...config.topicChannels, ...config.testChannels]);
  /** Thread facts per channel id; `null` means not a thread of a topic or test channel. */
  const threads = new Map<string, ThreadInfo | null>();
  const buffers = new Map<string, Map<string, Message>>();
  const due = new Map<string, number>();

  /** One message over REST; `null` when it is gone or unreadable. */
  const fetchMessage = async (channelId: string, id: string): Promise<Message | null> => {
    let value: unknown;
    try {
      value = await request<unknown>(`/channels/${channelId}/messages/${id}`);
    } catch (error) {
      if (unavailable(error)) return null;
      throw error;
    }
    const message = value as Message | null;
    if (!message || typeof message !== "object" || message.id !== id || message.channel_id !== channelId) {
      throw inboundProtocolError("Discord inbound message response is invalid");
    }
    return message;
  };
  const threadOf = async (channelId: string): Promise<ThreadInfo | undefined> => {
    if (homes.has(channelId)) return undefined;
    const cached = threads.get(channelId);
    if (cached !== undefined) return cached ?? undefined;
    const info = await channelInfo(request, channelId, true);
    const parent = info?.parent;
    const thread =
      parent === undefined || !homes.has(parent)
        ? null
        : {
            parentId: parent,
            ownerId: info?.owner ?? null,
            starterAuthorId: (await fetchMessage(parent, channelId))?.author?.id ?? null,
          };
    threads.set(channelId, thread);
    return thread ?? undefined;
  };

  const admit = async (channelId: string, store: DiscordInboundStore, messages: readonly Message[], emit: Emit) => {
    const cursor = store.load() ?? "0";
    const buffer = buffers.get(channelId) ?? new Map<string, Message>();
    for (const message of messages) buffer.set(message.id, message);
    for (const id of [...buffer.keys()]) if (!newer(id, cursor)) buffer.delete(id);
    buffers.set(channelId, buffer);
    due.delete(channelId);
    const thread = await threadOf(channelId);
    const scope = thread ? { thread } : {};
    let state = parseRouteState(store.loadRoute());
    const at = now();
    let assignee = state.assignee;
    const routed = [...buffer.values()].sort(byId).map((message) => {
      const before = assignee;
      const result = route(message, { ...scope, assignee: before, now: at }, config);
      if (assigns(result)) assignee = result.assignee ?? null;
      return { message, before, result };
    });
    const known: Neighbor[] = [
      ...state.recent,
      ...routed.map(({ message, result }) => ({ id: message.id, addressed: result.addressed, reason: result.reason })),
    ];
    for (const entry of routed) {
      if (entry.result.reason !== "unaddressed" && entry.result.reason !== "burst-pending") continue;
      const neighbors = known.filter((n) => n.id !== entry.message.id);
      entry.result = route(entry.message, { ...scope, assignee: entry.before, neighbors, now: at }, config);
    }
    for (const { message, result } of routed) {
      if (result.reason === "burst-pending") {
        due.set(channelId, result.decideAt ?? at);
        return;
      }
      if (result.addressed.includes(bot)) emit(toInbound(message));
      if (!result.reason.startsWith("ineligible")) {
        state = {
          assignee: assigns(result) ? (result.assignee ?? null) : state.assignee,
          recent: [...state.recent, { id: message.id, addressed: result.addressed, reason: result.reason }].slice(
            -RECENT_LIMIT,
          ),
        };
      }
      store.save(message.id, state);
      buffer.delete(message.id);
    }
    buffers.delete(channelId);
  };

  /** A gateway MESSAGE_UPDATE: only an edit that newly addresses this bot is handled, as a new request. */
  const update = async (event: Message, store: DiscordInboundStore, emit: Emit) => {
    if (typeof event.content !== "string" || typeof event.edited_timestamp !== "string") return;
    const buffer = buffers.get(event.channel_id);
    if (buffer?.has(event.id)) {
      const fresh = await fetchMessage(event.channel_id, event.id);
      if (fresh) buffer.set(event.id, fresh); // A held message is routed with its latest text.
      return;
    }
    const cursor = store.load();
    if (cursor === undefined || newer(event.id, cursor)) return; // Not read yet: history delivers the edited text.
    const state = parseRouteState(store.loadRoute());
    const index = state.recent.findIndex((entry) => entry.id === event.id);
    if (index < 0 || state.recent[index]?.addressed.includes(bot)) return;
    const fresh = await fetchMessage(event.channel_id, event.id);
    const editedAt = Date.parse(fresh?.edited_timestamp ?? "");
    if (!fresh?.edited_timestamp || !Number.isFinite(editedAt)) return;
    const thread = await threadOf(event.channel_id);
    const neighbors = state.recent.filter((entry) => entry.id !== event.id);
    const result = route(
      fresh,
      { ...(thread ? { thread } : {}), assignee: state.assignee, neighbors, now: now() },
      config,
    );
    if (result.reason === "burst-pending" || !result.addressed.includes(bot)) return;
    const edit = { id: `e:${bot}:${fresh.id}:${editedAt}`, editedAt: fresh.edited_timestamp };
    emit({ ...toInbound(fresh), edit });
    const entry = { id: fresh.id, addressed: result.addressed, reason: result.reason };
    store.save(cursor, { ...state, recent: state.recent.with(index, entry) });
  };

  return {
    admit,
    update,
    /** Channels with messages still waiting for a burst decision. */
    held: (): string[] => [...buffers.keys()],
    /** The earliest time a held burst decision becomes final, if any. */
    wakeAt: (): number | undefined => (due.size === 0 ? undefined : Math.min(...due.values())),
  };
};
