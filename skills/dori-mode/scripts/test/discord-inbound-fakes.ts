import { Discord } from "../src/messenger/discord.ts";
import type { DiscordInboundStore } from "../src/messenger/discord-inbound-state.ts";
import type { Http, HttpRequest } from "../src/messenger/http.ts";
import { fakeClock } from "./fakes.ts";

export type TestDiscordMessage = {
  id: string;
  channel_id: string;
  type: number;
  author: { id: string; bot?: boolean; system?: boolean };
  content?: string;
  webhook_id?: string;
  message_reference?: { message_id?: string };
};
export const ownerMessage = (id: string, patch: Partial<TestDiscordMessage> = {}): TestDiscordMessage => ({
  id,
  channel_id: "20",
  type: 0,
  author: { id: "10" },
  content: `owner ${id}`,
  ...patch,
});

export const memoryDiscordCursor = (initial?: string) => {
  let cursor = initial;
  const saved: string[] = [];
  const store: DiscordInboundStore = {
    load: () => cursor,
    save: (value) => {
      cursor = value;
      saved.push(value);
    },
  };
  return { store, saved };
};

/** Stateful history and authenticated REST endpoints, including overlapping/failed history pages. */
export const discordRestWorld = (messages: TestDiscordMessage[] = []) => {
  const state = {
    messages,
    bot: true,
    botId: "99",
    flags: 1 << 19,
    failBefore: false,
    overlap: false,
    stuckBefore: false,
    failure: new Map<string, number>(),
    unrelated: [] as TestDiscordMessage[],
  };
  const requests: HttpRequest[] = [];
  const http: Http = async (request) => {
    requests.push(request);
    const url = new URL(request.url);
    const path = url.pathname.slice("/api/v10".length);
    const status = state.failure.get(path);
    if (status) return { status, headers: {}, body: '{"diagnostic":"private unrelated server body"}' };
    if (request.headers?.["Authorization"] !== "Bot unit-private-token") {
      return { status: 401, headers: {}, body: '{"diagnostic":"private authentication body"}' };
    }
    let body: unknown;
    if (path === "/users/@me") body = { id: state.botId, bot: state.bot };
    else if (path === "/applications/@me") body = { flags: state.flags };
    else if (path === "/channels/20/messages") {
      const before = url.searchParams.get("before");
      if (before && state.failBefore) {
        return { status: 503, headers: {}, body: '{"diagnostic":"private second page body"}' };
      }
      const limit = Number(url.searchParams.get("limit"));
      const all = [...state.messages].sort((a, b) => (BigInt(a.id) > BigInt(b.id) ? -1 : 1));
      if (before && state.stuckBefore) body = all.slice(0, limit);
      else {
        const eligible = all.filter((message) => before === null || BigInt(message.id) < BigInt(before));
        const duplicate = before && state.overlap ? all.find((message) => message.id === before) : undefined;
        body = duplicate ? [duplicate, ...eligible.slice(0, limit - 1)] : eligible.slice(0, limit);
      }
      if (limit === 100) body = [...(body as TestDiscordMessage[]), ...state.unrelated];
    } else throw new Error(`unexpected test REST path ${path}`);
    return { status: 200, headers: {}, body: JSON.stringify(body) };
  };
  return { state, requests, discord: new Discord(http, fakeClock(0), "unit-private-token") };
};
