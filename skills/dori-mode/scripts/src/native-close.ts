import type { SessionRef } from "../../../../src/runtime/types.ts";
import type { FlowDeps } from "./done-flow.ts";
import type { LessonInput } from "./lessons.ts";
import { confirmedLaneState } from "./lifecycle-session.ts";
import { pendingDiscordLane } from "./messenger/discord-lane-inbox.ts";
import { type Lane, sameLaneGeneration, sessionOf, statusOf, withStatus } from "./registry.ts";
import { iso, runtimesOf } from "./run.ts";
import type { Check } from "./signals.ts";

export type CloseLesson = Pick<LessonInput, "rule" | "evidence">;
type CloseBasis = {
  readonly note: string;
  readonly checks: readonly Check[];
  readonly settlingClaim: boolean;
  readonly lesson?: CloseLesson;
};
export type NativeCloseIntent = CloseBasis & {
  readonly ref: SessionRef;
  readonly session: string;
  readonly epoch: string;
};

// A Done decision and a prepared close contend only on the short registry writer lock. Neither waits for pane I/O.
export const recordDoneDecision = async (
  deps: FlowDeps,
  seen: Lane,
  next: (current: Lane) => Lane,
): Promise<Lane | "closing" | null> => {
  let closing = false;
  const result = await deps.registry.update(seen.key, (current) => {
    closing = current?.nativeCloseIntent !== undefined;
    if (!current || closing || statusOf(current) === "closed" || !sameLaneGeneration(seen, current)) return null;
    const { threadClosePending: _waiting, ...active } = current;
    return next(active);
  });
  return closing ? "closing" : result;
};

const boundTo = (lane: Lane, intent: NativeCloseIntent): boolean => {
  const ref = sessionOf(lane);
  return (
    lane.lifecycleEpoch === intent.epoch &&
    lane.session === intent.session &&
    ref !== undefined &&
    ref.runtime === intent.ref.runtime &&
    ref.id === intent.ref.id &&
    ref.group === intent.ref.group &&
    ref.container === intent.ref.container
  );
};

type CloseCommit =
  | { readonly kind: "closed"; readonly lane: Lane; readonly pane: string | undefined }
  | { readonly kind: "changed"; readonly pane?: string | undefined }
  | { readonly kind: "held"; readonly reason: string };

const recordClosed = async (deps: FlowDeps, lane: Lane, basis: CloseBasis): Promise<CloseCommit> => {
  const intent = lane.nativeCloseIntent;
  const at = iso(deps.clock);
  const closed = await deps.registry.transition(lane, (current) => {
    const { nativeCloseIntent: stored, threadClosePending: _waiting, ...cleared } = current;
    if (intent ? !stored || !boundTo(current, stored) || !boundTo(current, intent) : stored) return null;
    const verified = basis.settlingClaim
      ? withStatus(cleared, "verified-done", "Done signals read back live", at)
      : cleared;
    return withStatus(verified, "closed", basis.note, at, { closedAt: at });
  });
  const pane = intent?.ref.id;
  return closed ? { kind: "closed", lane: closed, pane } : { kind: "changed", pane };
};

/** Under the delivery lock, prepare the exact pane close and commit only after an authoritative absence readback. */
export const commitClose = async (deps: FlowDeps, seen: Lane, basis: CloseBasis): Promise<CloseCommit> => {
  const saved = seen.nativeCloseIntent;
  if (seen.lifecycleEpoch === undefined && !saved) return recordClosed(deps, seen, basis);
  const hold = (reason: string): CloseCommit => ({ kind: "held", reason });
  const unconfirmed = "reopened pane identity or approval state is unconfirmed";
  let lane = seen;
  let intent = saved;
  if (intent) {
    if (!boundTo(lane, intent)) return hold("prepared close is for a different pane, session, or epoch");
  } else {
    if (lane.hold?.outcome) return hold(`terminal intent is already ${lane.hold.outcome}`);
    const ref = sessionOf(lane);
    const { lifecycleEpoch: epoch, session } = lane;
    if (!ref || !epoch || !session || !(await confirmedLaneState(deps, lane, ref))) return hold(unconfirmed);
    if (ref.runtime === "headless" && (await runtimesOf(deps).for(ref).probe(ref)).status !== "idle")
      return hold("native lane has not settled");
    if (pendingDiscordLane(deps.config.stateDir, lane)) return hold("Discord messages are still pending");
    const prepared = { ...basis, ref, session, epoch };
    const recorded = await deps.registry.transition(lane, (current) =>
      !current.nativeCloseIntent && boundTo(current, prepared) ? { ...current, nativeCloseIntent: prepared } : null,
    );
    if (!recorded) return { kind: "changed" };
    lane = recorded;
    intent = prepared;
  }

  const { ref } = intent;
  try {
    const runtime = runtimesOf(deps).for(ref);
    const isPresent = async () =>
      ref.runtime === "headless"
        ? (await runtime.probe(ref)).status !== "gone"
        : (await runtime.list()).some((s) => s.ref.id === ref.id);
    // An interrupted request may already have removed the target. Only a saved, still-bound intent may use that fact.
    if (!saved || (await isPresent())) {
      if (saved && pendingDiscordLane(deps.config.stateDir, lane)) return hold("Discord messages are still pending");
      if (!(await confirmedLaneState(deps, lane, ref))) return hold(unconfirmed);
      if (ref.runtime === "headless") await runtime.close(ref, "session", intent.session);
      else await runtime.close(ref, "session");
      if (await isPresent()) return hold(`pane ${ref.id} is still present`);
    }
  } catch (error) {
    return hold(`pane ${ref.id} close unconfirmed: ${String(error)}`);
  }
  return recordClosed(deps, lane, intent);
};
