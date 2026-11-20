import type { Runtimes } from "../../../../src/runtime/registry.ts";
import { isKind } from "../../../../src/runtime/hc/exec.ts";
import { reasonOf, ToolError } from "../../../../src/runtime/herdr/exec.ts";
import { mayRunOmpArgv } from "../../../../src/runtime/omp-argv.ts";
import type { SessionRef } from "../../../../src/runtime/types.ts";
import { finished } from "./boot-monitors.ts";
import { fill } from "./config.ts";
import type { FlowDeps } from "./done-flow.ts";
import { laneGoalActive } from "./goal-record.ts";
import type { LaunchInput } from "./launch-input.ts";
import { laneStartCommand, type OpenedLane } from "./launch-session.ts";
import type { Lane } from "./registry.ts";
import { Reports } from "./reports.ts";
import { type ChosenRuntimes, iso } from "./run.ts";
import { startupFailure, startupScreen } from "./startup-screen.ts";

const missingSession = (ref: SessionRef, error: unknown): boolean =>
  isKind(error, "not_found") ||
  (ref.runtime === "herdr" && error instanceof ToolError && /not.?found/i.test(reasonOf(error)));

export const launchFinished = async (deps: FlowDeps, key: string): Promise<boolean> => {
  const lane = await deps.registry.read(key);
  if (!lane) return false;
  const report = new Reports(deps.config.stateDir).read(key, { generation: lane.lifecycleEpoch });
  return finished({ ...lane, reportState: report?.state });
};

export const launchScreen = async (
  deps: FlowDeps,
  rts: Runtimes,
  ref: SessionRef,
  key: string,
): Promise<string | null> => {
  try {
    return await startupScreen(rts, ref);
  } catch (error) {
    if (missingSession(ref, error) && (await launchFinished(deps, key))) return null;
    throw error;
  }
};

/**
 * The runtime's own status word for the lane's session. Unlike agentStatus it keeps `shell` (no agent in the session)
 * apart from no status yet: herdr lists an agent that has not reported as `unknown` and a pane with no agent as `shell`.
 * hc lists both with no status, so a session listed with none is probed, which names a shell in front. A session missing
 * from the list is `gone`. OMP instead uses its live-process-bound probe, including approval and shell guards.
 */
export const launchStatus = async (
  rts: Runtimes,
  ref: SessionRef,
  ompAgent: boolean,
): Promise<{ status?: string; error?: string }> => {
  try {
    if (ompAgent) {
      const seen = await rts.for(ref).probe(ref);
      return { status: seen.status !== "gone" && seen.shellInFront ? "shell" : seen.status };
    }
    const found = (await rts.for(ref).list()).find((s) => s.ref.id === ref.id);
    if (!found) return { status: "gone" };
    if (found.status !== undefined) return { status: found.status };
    const probed = (await rts.for(ref).probe(ref)).status;
    return probed === "shell" || probed === "gone" ? { status: probed } : {};
  } catch (e) {
    if (missingSession(ref, e)) return { status: "gone" };
    return { error: e instanceof Error ? e.message : String(e) };
  }
};

// A fresh terminal receives its first check at 20 s, then every 10 s until startupWaitSeconds.
const FIRST_CHECK_MS = 20_000;
const RECHECK_MS = 10_000;
const ready = (s: string | undefined): boolean => s === "idle" || s === "working";
const unreported = (s: string | undefined): boolean => s === undefined || s === "unknown";
// Native OMP uses its current-session record. Custom agents may show a marker; an empty marker opts out of checking it.
const goalShown = (screen: string, marker: string): boolean =>
  screen
    .split("\n")
    .filter((s) => s.trim())
    .slice(-3)
    .join("\n")
    .includes(marker);

export const lastLaunchLines = (screen: string): string =>
  screen
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean)
    .slice(-3)
    .join(" / ")
    .replace(/\/(Users|home|Volumes|private)\/\S+/g, "(local path)")
    .slice(0, 240) || "(blank)";

export const startTerminalLane = async (
  deps: FlowDeps,
  input: LaunchInput,
  { lane, ref }: Extract<OpenedLane, { kind: "opened" }>,
  rts: ChosenRuntimes,
): Promise<{ readonly lane: Lane; readonly startup: string }> => {
  const model = input.model ?? deps.config.defaultModel;
  const cwd = input.cwd ?? deps.config.defaultCwd;
  const pane = ref.id;
  // Launch starts the activity journal; it is not a lane report and is never posted to the thread.
  new Reports(deps.config.stateDir).append({
    type: "activity",
    key: lane.key,
    at: iso(deps.clock),
    state: "running",
    text: `lane launched in ${pane}`,
  });
  const prompt = `${deps.config.launchKeywords}. Read and execute the lane brief at ${input.brief} in full. You are the ${input.key} lane; report as the brief's footer says.`;
  // The session can be written while start is still returning; its goal must belong to this invocation.
  const launchedAt = deps.clock.now();
  try {
    await rts.for(ref).start(ref, laneStartCommand(lane, fill(deps.config.agentCommand, { model, prompt })));
  } catch (e) {
    return { lane, startup: `STARTUP_ERROR ${input.key} ${pane}: start failed: ${lastLaunchLines(String(e))}` };
  }
  const started = deps.clock.now();
  await deps.clock.sleep(FIRST_CHECK_MS);
  const screen = await launchScreen(deps, rts, ref, input.key);
  if (screen === null) return { lane, startup: `STARTUP_DONE ${input.key}` };
  const ompAgent = mayRunOmpArgv(deps.config.agentCommand);
  const { status, error } = await launchStatus(rts, ref, ompAgent);
  const marker = deps.config.goalMarker;
  const omp = ompAgent ? { run: deps.run, rts, stateDir: deps.config.stateDir, ref } : undefined;
  const recordedGoal = () => laneGoalActive(deps.config.sessionsDir, cwd, input.key, launchedAt, omp);
  const hasGoal = async (shown: string): Promise<boolean> =>
    (!omp && goalShown(shown, marker)) || (await recordedGoal());
  // A working agent may quote a failure message; only a real goal vouches for it when a marker check is disabled.
  const bad = startupFailure(screen, ompAgent);
  const goalProven = async (): Promise<boolean> =>
    (marker !== "" && !omp && goalShown(screen, marker)) || (await recordedGoal());
  if (bad && !(status === "working" && !error && (await goalProven())))
    return { lane, startup: `STARTUP_ERROR ${input.key} ${pane}: ${bad}` };
  if (status === "gone" && (await launchFinished(deps, input.key)))
    return { lane, startup: `STARTUP_DONE ${input.key}` };
  // Unreported without a lookup error is a slow startup; a lookup error, shell, or other status ends the wait.
  if (ready(status) || (unreported(status) && !error)) {
    const rechecks = Math.max(0, Math.ceil((deps.config.startupWaitSeconds * 1000 - FIRST_CHECK_MS) / RECHECK_MS));
    let shown = screen;
    let current = status;
    for (let check = 0; !ready(current) || (marker && !(await hasGoal(shown))); check++) {
      if (check >= rechecks) {
        const waited = Math.round((deps.clock.now() - started) / 1000);
        return {
          lane,
          startup: ready(current)
            ? `STARTUP_NO_GOAL ${input.key} ${pane}: the agent is running but shows no goal after ${waited} s; last lines: ${lastLaunchLines(shown)}`
            : `STARTUP_UNCONFIRMED ${input.key} ${pane}: no ready agent status (${current ?? "none"}) after ${waited} s; last lines: ${lastLaunchLines(shown)}`,
        };
      }
      await deps.clock.sleep(RECHECK_MS);
      const nextScreen = await launchScreen(deps, rts, ref, input.key);
      if (nextScreen === null) return { lane, startup: `STARTUP_DONE ${input.key}` };
      shown = nextScreen;
      // The goal only counts while the agent is still running: re-check its status on every re-read.
      const again = await launchStatus(rts, ref, ompAgent);
      current = again.status;
      if (current === "gone" && (await launchFinished(deps, input.key)))
        return { lane, startup: `STARTUP_DONE ${input.key}` };
      if (again.error || !(ready(current) || unreported(current))) {
        const lost = again.error
          ? `status lookup failed: ${again.error}`
          : `agent status became ${again.status ?? "none"}`;
        return {
          lane,
          startup: `STARTUP_UNCONFIRMED ${input.key} ${pane}: ${lost} while waiting for its goal; last lines: ${lastLaunchLines(shown)}`,
        };
      }
      // Check for a late failure only in non-working agents without a goal, since working output may quote one.
      const failed = current !== "working" && !(await hasGoal(shown)) ? startupFailure(shown, ompAgent) : null;
      if (failed) return { lane, startup: `STARTUP_ERROR ${input.key} ${pane}: ${failed}` };
    }
    return { lane, startup: `STARTUP_OK ${input.key} ${pane}` };
  }
  const why = error ? `status lookup failed: ${error}` : `no ready agent status (${status ?? "none"})`;
  return { lane, startup: `STARTUP_UNCONFIRMED ${input.key} ${pane}: ${why}; last lines: ${lastLaunchLines(screen)}` };
};
