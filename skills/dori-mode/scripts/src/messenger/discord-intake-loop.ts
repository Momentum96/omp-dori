import type { DcMessage } from "./discord.ts";
import { DiscordFatalError, type GatewayHooks } from "./discord-intake-gateway.ts";
import type { DiscordIntake } from "./discord-intake.ts";

export type DcLoopDeps = {
  /** Holds the gateway session forever (runDiscordGateway with the token bound); throws only when fatal. */
  readonly gateway: (hooks: GatewayHooks) => Promise<never>;
  readonly sleep: (ms: number) => Promise<void>;
  readonly now: () => number;
  /** Prints the single INBOUND_READY line; called once. */
  readonly ready: (mode: "gateway" | "rest") => void;
  readonly warn: (line: string) => void;
};

/** Closes in a row before the loop also reads channels over REST, so a gateway outage does not hold messages. */
const REST_AFTER_FAILURES = 3;

/**
 * `dori inbound discord`. Without `--loop` it is one-shot: one REST catch-up of every visible channel, then it
 * returns. With `--loop MIN` it holds the gateway: messages are handled one at a time in arrival order; a fresh
 * session (READY, not RESUMED) first catches up over REST, retried every MIN until every channel was read; each
 * heartbeat ack of a caught-up live session moves the watermark. While the gateway keeps failing, and for good
 * when Discord refuses the Message Content intent (4014), the channels are read over REST every MIN instead.
 */
export const discordInboundLoop = async (inbound: DiscordIntake, loopMin: number, deps: DcLoopDeps): Promise<void> => {
  if (loopMin <= 0) {
    deps.ready("rest");
    await inbound.catchUp();
    return;
  }
  const everyMs = loopMin * 60_000;
  const fatal = Promise.withResolvers<never>();
  let queue: Promise<void> = Promise.resolve();
  // An emit failure (stdout gone) ends the watch with the cursor before that message: a restart re-delivers it.
  const enqueue = (task: () => Promise<void> | void): void => {
    queue = queue.then(task).catch((e: unknown) => fatal.reject(e));
  };
  let announced = false;
  let live = false;
  let pending = false;
  let lastTry = Number.NEGATIVE_INFINITY;
  const catchUp = async (): Promise<void> => {
    lastTry = deps.now();
    if (await inbound.catchUp()) pending = false;
  };
  const hooks: GatewayHooks = {
    onDispatch(type, d) {
      inbound.learnFrom(type, d);
      if (type === "READY" || type === "RESUMED") {
        live = true;
        if (type === "READY") {
          pending = true;
          enqueue(catchUp);
        }
        if (!announced) {
          announced = true;
          deps.ready("gateway");
        }
      } else if (type === "MESSAGE_CREATE") enqueue(() => inbound.message(d as DcMessage));
      else if (type === "MESSAGE_UPDATE") enqueue(() => inbound.edit(d as DcMessage));
    },
    onAck() {
      // Decided when the queue reaches it: a catch-up queued before this ack may have finished by then.
      if (live)
        enqueue(() => {
          if (!pending) inbound.mark();
          else if (deps.now() - lastTry >= everyMs) return catchUp();
        });
    },
    async onDown(failures) {
      live = false;
      if (failures >= REST_AFTER_FAILURES && deps.now() - lastTry >= everyMs) {
        enqueue(catchUp);
        await queue;
      }
    },
  };
  try {
    await Promise.race([deps.gateway(hooks), fatal.promise]);
  } catch (e) {
    if (!(e instanceof DiscordFatalError) || !e.message.includes(" 4014:")) throw e;
    deps.warn(`${e.message}; reading channels over REST every ${loopMin} min instead`);
  }
  await queue;
  if (!announced) deps.ready("rest");
  for (;;) {
    await catchUp();
    await deps.sleep(everyMs);
  }
};
