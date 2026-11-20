import { resolve } from "node:path";
import { configPath } from "./config.ts";
import { renderTemplate } from "./launch-template.ts";
import { type LaunchInput, validateLaunch } from "./launch-input.ts";
import type { FlowDeps } from "./done-flow.ts";
import { withLaunchLock } from "./delivery-lock.ts";
import { BRIEF_START, footer, originalBrief } from "./launch-footer.ts";
import { HeadlessRuntime, headlessArgv } from "../../../../src/runtime/headless.ts";
import { formatRef, parseRef } from "../../../../src/runtime/types.ts";
import { selectionFor } from "./launch-selection.ts";
import { openLaneSession, type OpenedLane } from "./launch-session.ts";
import { type Lane, sessionFields, sessionOf, upgradeLane } from "./registry.ts";
import { type ChosenRuntimes, iso, runtimesOf } from "./run.ts";
import { Reports } from "./reports.ts";
import { Lessons, renderLessons } from "./lessons.ts";
import { laneLaunchEnv, TokenFileError } from "./slack-token.ts";
import { lastLaunchLines, startTerminalLane } from "./launch-finished.ts";

export class LaunchError extends Error {}

export const adoptLane = async (
  deps: FlowDeps,
  input: LaunchInput & { readonly pane: string },
  locked = false,
): Promise<Lane> => {
  await validateLaunch(deps, input, LaunchError);
  if (!locked) {
    const held = await withLaunchLock(deps, input.key, () => adoptLane(deps, input, true));
    if (!held.ok) throw new LaunchError(`lane ${input.key} held: ${held.holder}`);
    return held.value;
  }
  const base: Lane = {
    key: input.key,
    title: input.title,
    thread: input.thread?.trim() ? input.thread : "none",
    brief: input.brief,
    done: input.done,
    cwd: input.cwd ?? deps.config.defaultCwd,
    openedAt: iso(deps.clock),
  };
  // `hc:t7` or `herdr:w1:p3` names its runtime; any other id is a herdr pane, as before runtimes existed.
  const ref = parseRef(input.pane);
  if (ref?.runtime === "headless") throw new LaunchError("headless lanes must be launched, not adopted");
  const lane = ref ? { ...base, ...sessionFields(ref) } : upgradeLane({ ...base, pane: input.pane });
  if (!(await deps.registry.create(lane))) throw new LaunchError(`lane ${input.key} is already registered`);
  return lane;
};

export const prepareLaunch = async (deps: FlowDeps, input: LaunchInput, queuedBrief?: string) => {
  await validateLaunch(deps, input, LaunchError);
  const cwd = input.cwd ?? deps.config.defaultCwd;
  let briefText: string;
  if (queuedBrief !== undefined) {
    briefText = queuedBrief;
  } else if (input.template) {
    briefText = renderTemplate(input.template, input.variables ?? {}, deps.env?.["DORI_CONFIG"] ?? configPath());
  } else {
    const briefFile = Bun.file(input.brief);
    if (!(await briefFile.exists())) throw new LaunchError(`brief not found: ${input.brief}`);
    briefText = originalBrief(await briefFile.text());
  }
  const draft: Lane = {
    key: input.key,
    title: input.title,
    thread: input.thread?.trim() ? input.thread : "none",
    brief: input.brief,
    done: input.done,
    cwd,
    model: input.model ?? deps.config.defaultModel,
    launchSelection: selectionFor(deps.config),
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
  const { lessons, problems, omitted } = new Lessons(deps.config.stateDir).forRepo(cwd);
  // A lesson the cap kept out is named too: a brief that silently lacks a standing rule is the bug this catches.
  const warnings = [
    ...problems.map((problem) => `LESSON_WARNING ${problem}`),
    ...omitted.map(
      (lesson) => `LESSON_OMITTED ${lesson.id} (${lesson.scope === "global" ? "global" : lesson.cwd}) ${lesson.rule}`,
    ),
  ];
  const { leadPane, repoIdentities, freshness } = deps.config;
  const laneFooter = footer(draft, leadPane, repoIdentities, runtimesOf(deps).choice?.kind, freshness);
  const preamble = `${renderLessons(lessons)}${BRIEF_START}\n${briefText}\n${laneFooter}`;
  return { lane: draft, briefText, preamble, warnings } as const;
};

export const launchLane = async (
  deps: FlowDeps,
  input: LaunchInput,
): Promise<{ readonly lane: Lane; readonly startup: string; readonly warnings: string[] }> => {
  await validateLaunch(deps, input, LaunchError);
  const held = await withLaunchLock(deps, input.key, async () => {
    const opened = await openLaunch(deps, input);
    return { ...(await opened.start()), warnings: opened.warnings };
  });
  if (!held.ok) throw new LaunchError(`lane ${input.key} held: ${held.holder}`);
  return held.value;
};

/** Caller holds the per-key launch lock; capacity admission only needs to span this phase. */
export const openLaunch = async (deps: FlowDeps, input: LaunchInput, queuedBrief?: string) => {
  const { lane: draft, preamble, warnings } = await prepareLaunch(deps, input, queuedBrief);
  const rts = runtimesOf(deps);
  const runtime = input.runtime ?? (deps.config.laneRuntime === "headless" ? "headless" : undefined);
  const group = runtime === "headless" ? undefined : deps.config.laneWorkspace || undefined;
  let argv: readonly string[] | undefined;
  if (runtime === "headless") {
    try {
      argv = headlessArgv(deps.config.agentCommand, input.model ?? deps.config.defaultModel);
    } catch (error) {
      throw new LaunchError(error instanceof Error ? error.message : String(error));
    }
  }
  let env: Record<string, string>;
  try {
    // Existing terminal launches take the operator's current configuration, not the daemon/runtime environment.
    const callerEnv = runtime === "headless" ? (deps.env ?? process.env) : process.env;
    env = laneLaunchEnv(callerEnv, callerEnv["DORI_CONFIG"] ?? configPath(), deps.config);
  } catch (e) {
    throw e instanceof TokenFileError ? new LaunchError(`token file refused: ${e.message}`) : e;
  }
  const cwd = input.cwd ?? deps.config.defaultCwd;
  const spec = { name: input.key, role: "lane", container: "tab", cwd, env } as const;
  const opened = await openLaneSession(deps.registry, rts, group === undefined ? spec : { ...spec, group }, draft, {
    preamble,
    generated: queuedBrief !== undefined || Boolean(input.template),
    ...(runtime === undefined ? {} : { runtime }),
  });
  if (opened.kind === "failed") {
    const why = opened.error instanceof Error ? opened.error.message : String(opened.error);
    throw new LaunchError(`${opened.what}: ${why}`);
  }
  if (opened.kind === "taken") throw new LaunchError(`lane ${input.key} is already registered`);
  return {
    warnings,
    start: () =>
      argv ? startHeadless(deps, input, opened, rts, argv, env) : startTerminalLane(deps, input, opened, rts),
  };
};

const startHeadless = async (
  deps: FlowDeps,
  input: LaunchInput,
  { lane, ref }: Extract<OpenedLane, { kind: "opened" }>,
  rts: ChosenRuntimes,
  argv: readonly string[],
  env: Record<string, string>,
): Promise<{ readonly lane: Lane; readonly startup: string }> => {
  const target = `${input.key} ${formatRef(ref)}`;
  const rt = rts.for(ref);
  let bound = lane;
  try {
    if (!(rt instanceof HeadlessRuntime)) throw new Error("headless lane runtime unavailable");
    const seen = await rt.startNative(ref, argv, env);
    if (!seen.sessionId || !seen.sessionFile) throw new Error("native identity missing");
    const { sessionId: session, sessionFile } = seen;
    // Record exact OMP identity before starting any turn: that turn may report or claim done immediately.
    const confirmed = await deps.registry.update(lane.key, (current) =>
      current && current.lifecycleEpoch === lane.lifecycleEpoch && sessionOf(current)?.id === ref.id && !current.session
        ? { ...current, session, sessionFile, sessionVia: "rpc-ui" }
        : null,
    );
    if (!confirmed) throw new Error("lane generation changed before native kickoff");
    bound = confirmed;
    new Reports(deps.config.stateDir).append({
      type: "activity",
      key: lane.key,
      at: iso(deps.clock),
      state: "running",
      text: `lane launched in ${formatRef(ref)}`,
      generation: ref.id,
    });
    await rt.begin(
      ref,
      session,
      `Read and execute the lane brief at ${resolve(input.brief)} in full. You are the ${input.key} lane; report as the brief's footer says. Done = ${input.done}`,
    );
    return { lane: bound, startup: `STARTUP_OK ${target}` };
  } catch (error) {
    // No retry: goal creation could have landed. Keep the registered and inspectable identity.
    return { lane: bound, startup: `STARTUP_UNCONFIRMED ${target}: ${lastLaunchLines(String(error))}` };
  }
};
