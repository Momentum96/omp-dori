import { afterEach, expect, test } from "bun:test";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { TgInbound } from "../src/messenger/telegram-inbound.ts";
import { inboundLagLog, inboundLagRecord, lagOfLine, summarizeLag, type InboundLagRecord } from "../src/messenger/telegram-lag.ts";
import { withState } from "./fakes.ts";

const base: TgInbound = { kind: "message", source: "dm", chat: 5250, updateId: 90, from: 5250, text: "hi" };

const state = withState();
afterEach(() => state.done());

/** Parse one jsonl line the way the reader must: bad or partial lines come back as undefined, never a throw. */
const parseRecord = (line: string): Partial<InboundLagRecord> | undefined => {
  try {
    const parsed: unknown = JSON.parse(line);
    if (parsed === null || typeof parsed !== "object") return undefined;
    return parsed;
  } catch {
    return undefined;
  }
};

test("lag is the gap between Telegram's date and the moment it was handled", () => {
  const record = inboundLagRecord({ ...base, date: 1_700_000_000 }, 1_700_000_002_250);
  expect(record.lagMs).toBe(2250);
  expect(record.tgDateMs).toBe(1_700_000_000_000);
  expect(record.at).toBe(new Date(1_700_000_002_250).toISOString());
});

test("a message with no date records no lag instead of a fake zero", () => {
  // Absence must stay absent: a 0 here would read as "arrived instantly" and drag every percentile down.
  const record = inboundLagRecord(base, 1_700_000_002_250);
  expect(record.lagMs).toBeUndefined();
  expect(record.tgDateMs).toBeUndefined();
  expect("lagMs" in record).toBe(false);
});

test("clock skew between host and Telegram clamps to zero, never a negative lag", () => {
  expect(inboundLagRecord({ ...base, date: 1_700_000_100 }, 1_700_000_000_000).lagMs).toBe(0);
});

test("each handled message appends one line carrying its identity", () => {
  const path = join(state.dir, "nested", "telegram-lag.jsonl");
  const record = inboundLagLog(path);
  record({ ...base, messageId: 7, date: 1_700_000_000 }, 1_700_000_001_000);
  record({ ...base, updateId: 91, date: 1_700_000_000 }, 1_700_000_004_000);
  const lines = readFileSync(path, "utf8").trim().split("\n");
  expect(lines.length).toBe(2);
  const [first, second] = lines.map(parseRecord);
  expect(first?.messageId).toBe(7);
  expect(first?.updateId).toBe(90);
  expect(first?.lagMs).toBe(1000);
  expect(second?.lagMs).toBe(4000);
});

test("an unwritable meter warns once and never throws into the receiver", () => {
  const warnings: string[] = [];
  const record = inboundLagLog(join(state.dir, "not-a-dir", "sub"), (m) => warnings.push(m));
  // Make the log path itself a directory so every append fails.
  mkdirSync(join(state.dir, "not-a-dir", "sub", "telegram-lag.jsonl"), { recursive: true });
  record(base, 1_000);
  record(base, 2_000);
  expect(warnings.length).toBe(1);
  expect(warnings[0]).toContain("unwritable");
});

test("percentiles are nearest-rank readings that actually occurred", () => {
  const summary = summarizeLag([5, 1, 4, 2, 3, 6, 7, 8, 9, 10]);
  expect(summary).toEqual({ n: 10, p50: 5, p90: 9, p99: 10, max: 10 });
  expect(summarizeLag([])).toBeUndefined();
});

test("lagOfLine skips blank, partial and non-record lines, keeping complete ones", () => {
  expect(lagOfLine('{"lagMs": 42}')).toBe(42);
  expect(lagOfLine('{"lagMs":')).toBeUndefined();
  expect(lagOfLine("")).toBeUndefined();
  expect(lagOfLine("null")).toBeUndefined();
  expect(lagOfLine('{"lagMs": "42"}')).toBeUndefined();
});

test("the summary counts only complete records when the tail is a partial line", () => {
  // Guards the readback path: a partial last line must be skipped, not crash the reader.
  const path = join(state.dir, "telegram-lag.jsonl");
  mkdirSync(state.dir, { recursive: true });
  writeFileSync(path, `${JSON.stringify({ lagMs: 100 })}\n${JSON.stringify({ lagMs: 300 })}\n{"lagMs":`);
  const lags = readFileSync(path, "utf8").split("\n").flatMap((line) => lagOfLine(line) ?? []);
  expect(summarizeLag(lags)?.n).toBe(2);
});
