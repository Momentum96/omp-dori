import { join } from "node:path";
import { UpdateError } from "./config.ts";
import { runCommand } from "./process.ts";
import { assertSource } from "./source.ts";
import { persistJob, saveJson } from "./state.ts";
import type { CommandReceipt, UpdateCommand, UpdateContext, UpdateJob } from "./types.ts";

/** The complete private report. Receipts count for a check only when bound to the exact checked source hash. */
export function candidateReport(ctx: UpdateContext, job: UpdateJob) {
  const latest = (name: string): CommandReceipt | undefined =>
    job.sourceHash === undefined
      ? undefined
      : job.commands.findLast((receipt) => receipt.name === name && receipt.sourceHash === job.sourceHash);
  const ran = (command: UpdateCommand, receipt: CommandReceipt | undefined) => ({
    name: command.name,
    exitCode: receipt?.exitCode ?? null,
    receipt: receipt ?? null,
  });
  // Preparation intentionally precedes the final source hash; only immutable checks attest that hash.
  const provision = ctx.config.provision.map((command) =>
    ran(
      command,
      job.commands.findLast((receipt) => receipt.name === command.name),
    ),
  );
  const checks = ctx.config.checks.map((command) => ran(command, latest(command.name)));
  if (
    job.phase === "ready" &&
    (!job.sourceHash ||
      checks.length === 0 ||
      [...provision, ...checks].some((item) => item.exitCode !== 0) ||
      job.review === undefined ||
      job.review.sourceHash !== job.sourceHash ||
      job.review.verdict !== "approve")
  ) {
    throw new UpdateError("ready candidate lacks passed checks and a fresh review bound to its source");
  }
  if (job.phase === "ready") assertSource(job);
  return {
    version: 1,
    target: "dori",
    candidateOnly: true,
    activationRequiresNewOwnerApproval: true,
    job: job.id,
    phase: job.phase,
    createdAt: job.createdAt,
    updatedAt: job.updatedAt,
    upstream: job.upstream,
    baseUpstream: job.baseUpstream,
    localSourceHash: job.inputs?.local.hash ?? null,
    upstreamSourceHash: job.inputs?.incoming.hash ?? null,
    baseSourceHash: job.inputs?.base.hash ?? null,
    candidateSourceHash: job.sourceHash ?? null,
    candidate: job.candidate,
    inputs: join(job.directory, "inputs", "manifest.json"),
    provision,
    checks,
    commands: job.commands,
    reviewer: job.review ?? null,
    port: job.port ?? null,
    problem: job.problem ?? null,
    notification: job.notification ?? null,
    rollback: {
      frozenLocalSource: join(job.directory, "inputs", "local"),
      frozenBaseUpstream: join(job.directory, "inputs", "base"),
      preparedSource: join(job.directory, "inputs", "upstream"),
    },
    limitations: [
      "Environment and path isolation is not an OS sandbox.",
      "No installation or live activation was performed.",
      "Readiness covers only the configured verification commands and the fresh review's stated limits.",
    ],
  };
}

/** Full paths/evidence stay in a private local artifact. Monitor/reporter output is a fixed, path-free projection. */
export function persistReport(ctx: UpdateContext, job: UpdateJob): void {
  saveJson(join(job.directory, "report.json"), candidateReport(ctx, job));
}

/** Classify a finished-or-killed reporter. A started process that did not exit cleanly may have delivered. */
function deliveryOf(receipt: CommandReceipt | undefined): NonNullable<UpdateJob["notification"]> {
  if (receipt === undefined || receipt.pid === undefined) return { state: "failed" };
  if (receipt.error !== undefined || receipt.signal !== undefined || receipt.exitCode === undefined)
    return { state: "ambiguous" };
  return { state: "failed", exitCode: receipt.exitCode };
}

/**
 * Records the candidate outcome and, when configured, sends one fixed path-free projection through the trusted
 * reporter. Intent is durable before the external write; a failed or ambiguous send is retained, never resent.
 */
export async function reportCandidate(ctx: UpdateContext, job: UpdateJob): Promise<void> {
  persistReport(ctx, job);
  ctx.emit({
    type: "DORI_UPDATE",
    target: "dori",
    event: "candidate",
    job: job.id,
    phase: job.phase,
    upstream: job.upstream,
  });
  if (!ctx.config.reporter || job.notification !== undefined || ctx.signal?.aborted) return;
  job.notification = { state: "sending" };
  persistJob(job);
  persistReport(ctx, job);
  const report = candidateReport(ctx, job);
  const text = JSON.stringify({
    type: "DORI_UPDATE",
    target: "dori",
    candidateOnly: true,
    job: job.id,
    phase: job.phase,
    upstream: job.upstream,
    baseUpstream: job.baseUpstream,
    localSourceHash: report.localSourceHash,
    candidateSourceHash: job.sourceHash ?? null,
    checkExits: report.checks.map((item) => item.exitCode),
    reviewer: job.review?.verdict ?? "unavailable",
    limitationCount: job.review?.limitations.length ?? 0,
    activationRequiresNewOwnerApproval: true,
  });
  const before = job.commands.length;
  try {
    const { receipt } = await runCommand(
      ctx,
      job,
      ctx.config.reporter,
      { text, job: job.id, upstream: job.upstream, phase: job.phase },
      "reporter",
    );
    job.notification = { state: "sent", exitCode: receipt.exitCode ?? 0 };
  } catch {
    job.notification = deliveryOf(job.commands.length > before ? job.commands.at(-1) : undefined);
    ctx.emit({
      type: "DORI_UPDATE",
      target: "dori",
      event: "report-delivery-held",
      job: job.id,
      phase: job.phase,
      upstream: job.upstream,
    });
  }
  persistJob(job);
  persistReport(ctx, job);
}
