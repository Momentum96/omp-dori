import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

const DISCORD_EPOCH = 1_420_070_400_000n;

/** The smallest snowflake Discord could mint at `ms`: ids after it were created after that moment. */
export const snowflakeAt = (ms: number): string =>
  ((BigInt(Math.max(0, Math.floor(ms))) - DISCORD_EPOCH) << 22n).toString();
/** Snowflakes order by numeric value; they outgrow Number, so compare as BigInt. */
export const snowflakeAfter = (a: string, b: string): boolean => BigInt(a) > BigInt(b);

/**
 * What `dori inbound discord` has delivered: per channel (a thread is its own channel), the newest message id whose
 * INBOUND line is out, and `upTo`, the moment up to which every visible channel is known to be processed. A restart
 * catches up each channel after max(its id, upTo minus a margin).
 */
export type DcCursor = { upTo?: string; readonly channels: Record<string, string> };
export type DcStore = { load(): DcCursor; save(cursor: DcCursor): void };

const SNOWFLAKE = /^[1-9]\d{0,19}$/;

export const fileDcStore = (path: string, warn: (line: string) => void): DcStore => ({
  load(): DcCursor {
    let raw: string;
    try {
      raw = readFileSync(path, "utf8");
    } catch {
      return { channels: {} }; // no file yet: a fresh start, nothing to catch up
    }
    try {
      const v = JSON.parse(raw) as { upTo?: unknown; channels?: unknown };
      const channels: Record<string, string> = {};
      for (const [k, id] of Object.entries((v.channels ?? {}) as Record<string, unknown>))
        if (SNOWFLAKE.test(k) && typeof id === "string" && SNOWFLAKE.test(id)) channels[k] = id;
      return typeof v.upTo === "string" && SNOWFLAKE.test(v.upTo) ? { upTo: v.upTo, channels } : { channels };
    } catch {
      // A corrupt cursor must not replay history: start fresh, as on a first install.
      warn(`discord-cursor at ${path} is unparseable; starting fresh without catch-up`);
      return { channels: {} };
    }
  },
  save(cursor: DcCursor): void {
    mkdirSync(dirname(path), { recursive: true });
    const tmp = `${path}.${process.pid}.tmp`;
    writeFileSync(tmp, JSON.stringify(cursor));
    renameSync(tmp, path);
  },
});

/** Discord ids in env and flags: digits only, no sign, no exponent; a mistyped id fails at start, naming it. */
export const parseSnowflake = (name: string, raw: string | undefined): string => {
  if (raw === undefined) throw new Error(`${name} is not set`);
  const v = raw.trim();
  if (!SNOWFLAKE.test(v) || BigInt(v) >= 1n << 64n)
    throw new Error(`${name} must be a Discord id (digits only), got: ${JSON.stringify(raw)}`);
  return v;
};
