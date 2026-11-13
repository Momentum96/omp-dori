/**
 * GPT-6.1 Sol pays a slow provider round trip for every response and, in a lane, spends about half of its responses on
 * bookkeeping alone: one todo update, one goal call, one footer command (checkpoint, report, forwarding the [REPORT]
 * line, claim-done) each. A lane's main agent on Sol is told to carry that bookkeeping along with real work instead.
 * Other models, the Dori session, subagents and plain sessions are left unchanged.
 */

export interface PromptModel {
  id: string;
  name?: string;
}

const MARKER = "<omp-dori-lane-bookkeeping>";
const SOL = /(^|[/@:._\s-])gpt[._\s-]?6(?:[._\s-]1|1)[._\s-]sol(?=$|[/@:._\s-])/i;

export const LANE_BOOKKEEPING = `${MARKER}
## Lane bookkeeping rides along with work

Every response on this model is a slow provider round trip. In lanes about half of all responses do nothing but bookkeeping: one todo update, one goal call, or one footer command (checkpoint, report, forwarding the [REPORT] line, claim-done), each a response of its own. Bookkeeping never gets a response of its own:
- You can emit several tool calls in one response. When a step finishes, send its todo update, its footer checkpoint and the next step's first real call together in that response. Goal create goes out beside the todo init, goal complete beside the final footer call. Inside an \`eval\` cell, \`tool.todo(...)\` and \`tool.goal(...)\` work too.
- Chain the footer commands due at the same point into one \`bash\` call with \`&&\`: checkpoint, report, forwarding its [REPORT] line, claim-done.
- Close out in one response: the final footer chain, the last todo update and goal complete together.
</omp-dori-lane-bookkeeping>`;

export const isSol = (model: PromptModel | undefined): boolean =>
  model !== undefined && (SOL.test(model.id) || (model.name !== undefined && SOL.test(model.name)));

/** The system prompt with the bookkeeping block appended, or undefined when it does not apply or is already present. */
export const withLaneBookkeeping = (systemPrompt: unknown, model: PromptModel | undefined): string[] | undefined => {
  if (!Array.isArray(systemPrompt) || !systemPrompt.every((block) => typeof block === "string")) return undefined;
  if (!isSol(model) || systemPrompt.some((block: string) => block.includes(MARKER))) return undefined;
  return [...systemPrompt, LANE_BOOKKEEPING];
};
