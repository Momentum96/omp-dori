import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { parseArgs } from "node:util";
import { UpdateError } from "./config.ts";
import { DoriUpdater } from "./controller.ts";
import { loadUpdateConfig } from "./load.ts";
import { acknowledgeStopped, restoreUpdate, snapshotUpdate } from "./recovery.ts";
import { persistReport } from "./report.ts";
import { saveJson } from "./state.ts";

const COMMANDS = ["check", "stage", "watch", "status", "snapshot", "restore", "recover"];
const USAGE = `dori update <check|stage|watch|status|snapshot|restore|recover> --config FILE
  check       detect a changed configured Dori ref and stage it once (the configured baseline is skipped)
  stage       explicitly stage the current pinned ref, including the configured baseline (deduplicated)
  watch       keep checking; stop and retain ambiguous/interrupted work
  status      print private local JSON state and candidate reports
  snapshot    --job ID --output NEW_DIR [--app-state DIR]: freeze code, control evidence and explicit candidate app data
  restore     --input SNAPSHOT_DIR --output NEW_DIR: restore an isolated recovery copy, never readiness
  recover     --ack-stopped ID|none: acknowledge every old child stopped; clear the active job or idle controller lock
No installation, commit, push, global upgrade, ULW update or live cutover.`;

/** Exit codes: 0 ok/unchanged/ready, 3 held/failed/rejected candidate or failed report, 2 operator error, 130 interrupted. */
export async function runUpdate(argv: readonly string[]): Promise<number> {
  let updater: DoriUpdater | undefined;
  const stop = new AbortController();
  const interrupt = (): void => stop.abort();
  try {
    const args = parseArgs({
      args: [...argv],
      allowPositionals: true,
      strict: true,
      options: {
        config: { type: "string" },
        job: { type: "string" },
        output: { type: "string" },
        input: { type: "string" },
        "ack-stopped": { type: "string" },
        "app-state": { type: "string" },
        help: { type: "boolean" },
      },
    });
    if (args.values.help || args.positionals.length === 0) {
      console.log(USAGE);
      return 0;
    }
    const command = args.positionals[0] ?? "";
    if (args.positionals.length !== 1 || !COMMANDS.includes(command)) throw new UpdateError(USAGE);
    const need = (value: string | undefined, name: string): string => {
      if (!value) throw new UpdateError(`update ${command} requires --${name}`);
      return value;
    };
    updater = new DoriUpdater(loadUpdateConfig(resolve(need(args.values.config, "config"))), undefined, stop.signal);
    const ctx = updater.context;
    process.on("SIGTERM", interrupt);
    process.on("SIGINT", interrupt);
    if (command === "status") {
      console.log(JSON.stringify(updater.status(), null, 2));
      return 0;
    }
    if (command === "snapshot") {
      const snapshot = snapshotUpdate(
        ctx,
        need(args.values.job, "job"),
        need(args.values.output, "output"),
        args.values["app-state"],
      );
      console.log(JSON.stringify({ type: "DORI_UPDATE_RECOVERY", snapshot }));
      return 0;
    }
    if (command === "restore") {
      const restored = restoreUpdate(ctx, need(args.values.input, "input"), need(args.values.output, "output"));
      console.log(JSON.stringify({ type: "DORI_UPDATE_RECOVERY", restored, phase: "interrupted" }));
      return 0;
    }
    if (command === "recover") {
      const job = acknowledgeStopped(ctx, need(args.values["ack-stopped"], "ack-stopped"));
      if (job) persistReport(ctx, job);
      return 0;
    }
    if (command === "watch") {
      const outcome = await updater.watch();
      return stop.signal.aborted ? 130 : outcome === "held" ? 3 : 0;
    }
    const result = await updater.once(command === "stage" ? "stage" : "check");
    const sent = result.job?.notification?.state;
    const unfinished = result.job !== undefined && result.job.phase !== "ready";
    return result.outcome === "held" || unfinished || sent === "failed" || sent === "ambiguous" ? 3 : 0;
  } catch (error) {
    if (updater && existsSync(updater.context.config.root)) {
      // Diagnostics are private and must never hide the original failure.
      saveJson(resolve(updater.context.config.root, "cli-error.json"), {
        at: new Date().toISOString(),
        error: error instanceof Error ? error.message : String(error),
      });
    }
    const visible =
      error instanceof UpdateError || String((error as NodeJS.ErrnoException).code).startsWith("ERR_PARSE_ARGS");
    console.error(
      JSON.stringify({
        type: "DORI_UPDATE",
        target: "dori",
        event: "error",
        reason:
          visible && error instanceof Error ? error.message : "operation failed; inspect dedicated updater artifacts",
      }),
    );
    return stop.signal.aborted ? 130 : 2;
  } finally {
    process.removeListener("SIGTERM", interrupt);
    process.removeListener("SIGINT", interrupt);
  }
}
