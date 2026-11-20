const TEXT_LIMIT = 1536;

export function remoteRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function remoteText(text: string) {
  return { text: text.slice(0, TEXT_LIMIT), ...(text.length > TEXT_LIMIT ? { truncated: true } : {}) };
}

function blocks(value: unknown): string[] {
  if (typeof value === "string") return [value];
  if (!Array.isArray(value)) return [];
  return value.flatMap((part: unknown) =>
    remoteRecord(part) && part.type === "text" && typeof part.text === "string" ? [part.text] : [],
  );
}

/** Visible text only: no reasoning, credentials, raw tool inputs/results, or private extension context. */
export function remoteMessage(value: unknown) {
  if (!remoteRecord(value)) return;
  const { role, content, timestamp, stopReason } = value;
  if (role !== "user" && role !== "assistant" && !(role === "custom" && value.display === true)) return;
  const text = blocks(content).join("\n");
  return {
    role,
    ...remoteText(text),
    ...(typeof timestamp === "number" && Number.isFinite(timestamp) ? { timestamp } : {}),
    ...(role === "assistant" && typeof stopReason === "string" ? { stopReason: stopReason.slice(0, 32) } : {}),
  };
}

export function remoteTranscript(branch: readonly unknown[], limit: number) {
  const messages = [];
  for (let i = branch.length - 1; i >= 0 && messages.length < limit; i--) {
    const entry = branch[i];
    if (!remoteRecord(entry) || typeof entry.id !== "string") continue;
    if (entry.type === "reset_boundary") break;
    const message = remoteMessage(
      entry.type === "message"
        ? entry.message
        : entry.type === "custom_message"
          ? { ...entry, role: "custom" }
          : undefined,
    );
    if (message) messages.push({ id: entry.id, ...message });
  }
  return messages.reverse();
}

/** Distinguish observed native output from a verified durable Cataly report. */
export function nativeReportLines(event: Record<string, unknown>) {
  if (event.isError !== false || !remoteRecord(event.result)) return [];
  return blocks(event.result.content)
    .flatMap((text) => Bun.stripANSI(text).split(/\r?\n/))
    .filter((line) => /^\[REPORT\] [\w.-]+ \| (milestone|blocker|question|halted|done) \| \S/.test(line))
    .slice(0, 8);
}
