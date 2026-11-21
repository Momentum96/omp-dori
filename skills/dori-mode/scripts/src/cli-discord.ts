/** The Discord Gateway plumbing shared by `dori presence discord` and `dori inbound discord --gateway`. */
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { die, every } from "./cli-context.ts";
import { Discord } from "./messenger/discord.ts";
import type { GatewayClock, GatewaySocket } from "./messenger/discord-gateway.ts";
import { discordInbound, formatDiscordInbound } from "./messenger/discord-inbound.ts";
import { discordInboundLive } from "./messenger/discord-inbound-live.ts";
import type { DiscordRouting } from "./messenger/discord-inbound-routing.ts";
import { discordInboundEnv, fileDiscordInboundStore } from "./messenger/discord-inbound-state.ts";
import { routeConfigFromTopology } from "./messenger/discord-route.ts";
import { fetchHttp } from "./messenger/http.ts";
import { realClock } from "./run.ts";

export const openDiscordSocket = (url: string): GatewaySocket => {
  const ws = new WebSocket(url);
  return {
    send: (data) => ws.send(data),
    close: (code) => ws.close(code),
    onMessage: (cb) => ws.addEventListener("message", (event) => cb(String(event.data))),
    onClose: (cb) => ws.addEventListener("close", (event) => cb(event.code)),
    onError: (cb) => ws.addEventListener("error", () => cb()),
  };
};

export const discordClock: GatewayClock = {
  now: realClock.now,
  random: Math.random,
  setTimeout: (cb, ms) => setTimeout(cb, ms),
  clearTimeout: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
};

/** Waits for a Gateway connection to end, closing it on SIGINT or SIGTERM. */
export const untilStopped = async (gateway: { done: Promise<void>; close(): void }): Promise<void> => {
  const stop = () => gateway.close();
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);
  try {
    await gateway.done;
  } finally {
    process.removeListener("SIGINT", stop);
    process.removeListener("SIGTERM", stop);
    gateway.close();
  }
};

const idList = (value: string | undefined): string[] =>
  (value ?? "")
    .split(",")
    .map((id) => id.trim())
    .filter((id) => id !== "");

/** DORI_BOT=omp turns on mention routing under the topology at DORI_TOPOLOGY (default ~/.dori/shared/node/topology.json). */
const discordRouting = (env: NodeJS.ProcessEnv): DiscordRouting | undefined => {
  const bot = env["DORI_BOT"]?.trim();
  if (bot === undefined || bot === "") return undefined;
  if (bot !== "omp") return die("DORI_BOT must be omp or unset");
  const path = env["DORI_TOPOLOGY"]?.trim() || join(homedir(), ".dori/shared/node/topology.json");
  try {
    return { bot, config: routeConfigFromTopology(JSON.parse(readFileSync(path, "utf8"))), now: realClock.now };
  } catch (error) {
    return die(`DORI_TOPOLOGY ${path} is unusable: ${error instanceof Error ? error.message : String(error)}`);
  }
};

/** Channels come from DORI_DISCORD_CHANNEL_ID, DORI_DISCORD_CHANNEL_IDS (comma separated), DORI_DISCORD_SCOPE=all and threads under watched channels unless DORI_DISCORD_THREADS=off. */
export const discordInboundCommand = async (stateDir: string, gateway: boolean, pollMinutes: number) => {
  const env = process.env;
  const ownerId = discordInboundEnv("DORI_DISCORD_OWNER_ID", env["DORI_DISCORD_OWNER_ID"]);
  const scope = env["DORI_DISCORD_SCOPE"]?.trim();
  if (scope !== undefined && scope !== "" && scope !== "all") die("DORI_DISCORD_SCOPE must be all or unset");
  const all = scope === "all";
  const setting = env["DORI_DISCORD_THREADS"]?.trim().toLowerCase();
  if (setting !== undefined && setting !== "" && setting !== "off" && setting !== "on")
    die("DORI_DISCORD_THREADS must be on, off or unset");
  const threads = setting !== "off";
  const channelIds = ["DORI_DISCORD_CHANNEL_ID", "DORI_DISCORD_CHANNEL_IDS"].flatMap((name) =>
    idList(env[name]).map((id) => discordInboundEnv(name, id)),
  );
  if (!all && channelIds.length === 0) discordInboundEnv("DORI_DISCORD_CHANNEL_ID", env["DORI_DISCORD_CHANNEL_ID"]);
  const token = env["DORI_DISCORD_TOKEN"] || die("DORI_DISCORD_TOKEN is not set");
  const routing = discordRouting(env);
  const discord = new Discord(fetchHttp, realClock, token);
  const intake = await discordInbound(discord, (channelId) => fileDiscordInboundStore(stateDir, channelId), {
    ownerId,
    channelIds: [...new Set(channelIds)],
    all,
    threads,
    ...(routing ? { routing } : {}),
  });
  const watching = all ? "all" : intake.channels().join(",");
  let reactWarned = false;
  const emit = (message: Parameters<typeof formatDiscordInbound>[0]) => {
    console.log(formatDiscordInbound(message));
    // Read receipt, as on Telegram: every emitted message is addressed to this bot, so it gets the eyes on intake and
    // keeps them after the reply. A failed reaction never costs the message; it is warned once and dropped.
    void discord.react(message.channel, message.message, "\u{1f440}").catch((e: unknown) => {
      if (reactWarned) return;
      reactWarned = true;
      console.log(
        `WARN reaction failed (${e instanceof Error ? e.message : String(e)}); still answering`.slice(0, 300),
      );
    });
  };
  console.log(
    `INBOUND_READY discord owner=${ownerId} channel=${watching} bot=${intake.botId}${gateway ? " mode=gateway" : ""}${routing ? ` routing=${routing.bot}` : ""}`,
  );
  if (!gateway) return await every(pollMinutes, () => intake.poll(emit));
  await untilStopped(discordInboundLive(intake, openDiscordSocket, token, discordClock, emit, (l) => console.log(l)));
};
