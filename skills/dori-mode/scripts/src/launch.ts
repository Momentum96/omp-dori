import { fill, isOmpCommand } from "./config.ts";
import type { FlowDeps } from "./done-flow.ts";
import { laneGoalActive } from "./goal-record.ts";
import { BRIEF_START, footer, originalBrief, quoteForPane } from "./launch-footer.ts";
import { parseRef, type SessionRef } from "../../../../src/runtime/types.ts";
import { LANE_KEY, type Lane, RESERVED_LANE_KEY, sessionFields, upgradeLane } from "./registry.ts";
import { iso, runtimesOf } from "./run.ts";
import { readScreen, statusLookup } from "./sessions.ts";
import { Reports } from "./reports.ts";
import { doneSyntaxErrors } from "./signals.ts";
import type { BudgetLimits } from "./supervision-observe.ts";
import { Lessons, renderLessons } from "./lessons.ts";

export type LaunchInput = {
  readonly key: string;
  readonly title: string;
  readonly brief: string;
  readonly done: string;
  readonly thread?: string;
  readonly model?: string;
  readonly cwd?: string;
  readonly budget?: BudgetLimits;
  readonly activityPaths?: readonly string[];
};

export class LaunchError extends Error {}

const STARTUP_FAILURE =
  /Cannot find module|All fallback models failed|No API key|model_not_available|usage limit|rate limit|quota|Session crashed|unknown provider|Failed to load extension|Trusted extension failed to load|unknown flags?: |Model \S+ not found|No models available/i;
// OMP goals are read from its current live session record. A custom agent may use its configured footer marker;
// neither path accepts status text quoted earlier in the agent's output.
const GOAL_CHECKS = 6;
const goalShown = (screen: string, marker: string): boolean =>
  screen
    .split("\n")
    .filter((s) => s.trim())
    .slice(-3)
    .join("\n")
    .includes(marker);

export const validateLaunch = async (deps: FlowDeps, input: LaunchInput): Promise<void> => {
  if (!LANE_KEY.test(input.key)) throw new LaunchError(`key must match ${LANE_KEY}`);
  if (input.key === RESERVED_LANE_KEY)
    throw new LaunchError(`lane key "${RESERVED_LANE_KEY}" is reserved for the lead`);
  if (await deps.registry.read(input.key)) throw new LaunchError(`lane ${input.key} is already registered`);
  const errors = doneSyntaxErrors(input.done);
  if (errors.length) throw new LaunchError(`Done line is not checkable: ${errors.join("; ")}`);
  for (const [name, value] of Object.entries(input.budget ?? {})) {
    if (value != null && (!Number.isFinite(value) || value < 0)) throw new LaunchError(`invalid ${name} ceiling`);
  }
};

export const adoptLane = async (deps: FlowDeps, input: LaunchInput & { readonly pane: string }): Promise<Lane> => {
  await validateLaunch(deps, input);
  const base: Lane = {
    key: input.key,
    title: input.title,
    thread: input.thread ?? "none",
    brief: input.brief,
    done: input.done,
    cwd: input.cwd ?? deps.config.defaultCwd,
    openedAt: iso(deps.clock),
  };
  // `hc:t7` or `herdr:w1:p3` names its runtime; any other id is a herdr pane, as before runtimes existed.
  const ref = parseRef(input.pane);
  const lane = ref ? { ...base, ...sessionFields(ref) } : upgradeLane({ ...base, pane: input.pane });
  await deps.registry.write(lane);
  return lane;
};

export const prepareLaunch = async (
  deps: FlowDeps,
  input: LaunchInput,
): Promise<{ readonly lane: Lane; readonly preamble: string; readonly warnings: string[] }> => {
  await validateLaunch(deps, input);
  const model = input.model ?? deps.config.defaultModel;
  const cwd = input.cwd ?? deps.config.defaultCwd;
  const briefFile = Bun.file(input.brief);
  if (!(await briefFile.exists())) throw new LaunchError(`brief not found: ${input.brief}`);
  const draft: Lane = {
    key: input.key,
    title: input.title,
    thread: input.thread ?? "none",
    brief: input.brief,
    done: input.done,
    cwd,
    model,
    openedAt: iso(deps.clock),
    reportProtocol: "jsonl-v1",
    budget: {
      ...deps.config.supervision.defaults,
      ...deps.config.supervision.lanes?.[input.key],
      // An undefined field is absent, so it keeps the configured ceiling; only null turns one off.
      ...Object.fromEntries(Object.entries(input.budget ?? {}).filter(([, value]) => value !== undefined)),
    },
    activityPaths: input.activityPaths ?? [],
  };
  // A damaged journal line is a warning: it must not keep a lane from starting or hide the lessons around it.
  const { lessons, problems } = new Lessons(deps.config.stateDir).forRepo(cwd);
  return {
    lane: draft,
    warnings: problems.map((problem) => `LESSON_WARNING ${problem}`),
    preamble: `${renderLessons(lessons)}${BRIEF_START}\n${originalBrief(await briefFile.text())}\n${footer(draft, deps.config.leadPane, deps.config.repoIdentities)}`,
  };
};

export const launchLane = async (
  deps: FlowDeps,
  input: LaunchInput,
): Promise<{ readonly lane: Lane; readonly startup: string; readonly warnings: string[] }> => {
  const prepared = await prepareLaunch(deps, input);
  return { ...(await startLane(deps, input, prepared.lane, prepared.preamble)), warnings: prepared.warnings };
};

const startLane = async (
  deps: FlowDeps,
  input: LaunchInput,
  draft: Lane,
  preamble: string,
): Promise<{ readonly lane: Lane; readonly startup: string }> => {
  const model = input.model ?? deps.config.defaultModel;
  const cwd = input.cwd ?? deps.config.defaultCwd;
  await Bun.write(input.brief, preamble);
  const rts = runtimesOf(deps);
  const group = deps.config.laneWorkspace || undefined;
  let ref: SessionRef;
  try {
    const spec = { name: input.key, role: "lane", container: "tab", cwd, env: {} } as const;
    ref = await rts.default.create(group === undefined ? spec : { ...spec, group });
  } catch (e) {
    throw new LaunchError(`session create failed: ${e instanceof Error ? e.message : String(e)}`);
  }
  const pane = ref.id;
  // Registered before the agent starts: dori report refuses an unknown key, so a fast lane's first report would be lost.
  const lane: Lane = { ...draft, ...sessionFields(ref) };
  await deps.registry.write(lane);
  // The launch is an activity record, not a report: it starts the journal (so the screen is watched for a stall) but is
  // never posted to the thread as if the agent had reported.
  new Reports(deps.config.stateDir).append({
    type: "activity",
    key: lane.key,
    at: iso(deps.clock),
    state: "running",
    text: `lane launched in ${pane}`,
  });
  const prompt = `${deps.config.launchKeywords}. Read and execute the lane brief at ${input.brief} in full. You are the ${input.key} lane; report as the brief's footer says.`;
  const started = deps.clock.now();
  try {
    await rts.for(ref).start(ref, fill(deps.config.agentCommand, { model, prompt }).map(quoteForPane).join(" "));
  } catch (e) {
    return { lane, startup: `STARTUP_ERROR ${input.key} ${pane}: start failed: ${lastLines(String(e))}` };
  }
  await deps.clock.sleep(20_000);
  const screen = await readScreen(rts, ref, 40);
  const bad = STARTUP_FAILURE.exec(screen);
  if (bad) return { lane, startup: `STARTUP_ERROR ${input.key} ${pane}: ${bad[0]}` };
  const ompAgent = isOmpCommand(deps.config.agentCommand);
  const startupStatus = async (): Promise<{ readonly status?: string; readonly error?: string }> => {
    if (!ompAgent) return statusLookup(rts, ref);
    try {
      const probe = await rts.for(ref).probe(ref);
      return { status: probe.shellInFront ? "shell" : probe.status };
    } catch (e) {
      return { error: e instanceof Error ? e.message : String(e) };
    }
  };
  const { status, error } = await startupStatus();
  if (status === "idle" || status === "working") {
    const marker = deps.config.goalMarker;
    const omp = ompAgent ? { run: deps.run, rts, stateDir: deps.config.stateDir, ref } : undefined;
    const hasGoal = async (shown: string): Promise<boolean> =>
      (!omp && goalShown(shown, marker)) ||
      (await laneGoalActive(deps.config.sessionsDir, cwd, input.key, started, omp));
    let shown = screen;
    for (let check = 0; marker && !(await hasGoal(shown)); check++) {
      if (check === GOAL_CHECKS)
        return {
          lane,
          startup: `STARTUP_NO_GOAL ${input.key} ${pane}: the agent is running but shows no goal after ${Math.round((deps.clock.now() - started) / 1000)} s; last lines: ${lastLines(shown)}`,
        };
      await deps.clock.sleep(10_000);
      shown = await readScreen(rts, ref, 40);
      // The goal is only good news while the agent is still running: re-check the status on every re-read.
      const again = await startupStatus();
      if (again.status !== "idle" && again.status !== "working") {
        const lost = again.error
          ? `status lookup failed: ${again.error}`
          : `agent status became ${again.status ?? "none"}`;
        return {
          lane,
          startup: `STARTUP_UNCONFIRMED ${input.key} ${pane}: ${lost} while waiting for its goal; last lines: ${lastLines(shown)}`,
        };
      }
      // A late failure stops the agent. A working agent's own output may quote these phrases (a file or log it reads),
      // so only an idle agent with no goal is checked for one.
      const failed = again.status === "idle" && !(await hasGoal(shown)) ? STARTUP_FAILURE.exec(shown) : null;
      if (failed) return { lane, startup: `STARTUP_ERROR ${input.key} ${pane}: ${failed[0]}` };
    }
    return { lane, startup: `STARTUP_OK ${input.key} ${pane}` };
  }
  const why = error ? `status lookup failed: ${error}` : `no ready agent status (${status ?? "none"})`;
  return { lane, startup: `STARTUP_UNCONFIRMED ${input.key} ${pane}: ${why}; last lines: ${lastLines(screen)}` };
};

const lastLines = (screen: string): string =>
  screen
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean)
    .slice(-3)
    .join(" / ")
    .replace(/\/(Users|home|Volumes|private)\/\S+/g, "(local path)")
    .slice(0, 240) || "(blank)";
