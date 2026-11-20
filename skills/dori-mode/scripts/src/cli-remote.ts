import { randomUUID } from "node:crypto";
import { parseArgs } from "node:util";
import { object, type Request } from "../../../../src/remote/client.ts";
import { remotePolicy, setRemotePolicy } from "../../../../src/remote/policy.ts";
import { peer, throughPeer } from "../../../../src/remote/transport.ts";
import { loadConfig } from "./config.ts";
import type { FlowDeps } from "./done-flow.ts";
import { Registry } from "./registry.ts";
import { acceptRemote, remoteOperation } from "./remote-worker.ts";
import { realClock, run } from "./run.ts";

const usage = `dori remote <sessions|read|send|receipt|watch|interrupt|model|close|attach|open|wake> [SESSION]
  [--on HC_MACHINE] [--via /absolute/path/to/target/dori] [--generation LIVE_GENERATION]
  send SESSION --text TEXT [--steer] [--id REQUEST_ID] [--no-wait]
  receipt SESSION --id REQUEST_ID
  read SESSION [--limit N]
  watch SESSION [--cursor N] [--until-completion]
  model SESSION --model PROVIDER/ID
  close SESSION [--force]
  attach SESSION [--observe]
  open [--cwd DIR] [--name NAME] [--model MODEL] [--thinking LEVEL] [--text INITIAL_PROMPT]
       [--worktree PATH [--branch BRANCH] [--base REF]] [--id REQUEST_ID]
  wake SESSION (exact saved ID)
  policy [control|wake on|off] (change on the target itself)`;

type Call = (request: Request) => Promise<unknown>;
const flagsFor: Readonly<Record<string, readonly string[]>> = {
  sessions: [],
  read: ["limit"],
  send: ["text", "steer", "id", "no-wait"],
  receipt: ["id"],
  watch: ["cursor", "until-completion"],
  interrupt: [],
  model: ["model"],
  close: ["force"],
  attach: ["observe", "cursor"],
  open: ["cwd", "name", "model", "thinking", "text", "worktree", "branch", "base", "id"],
  wake: [],
};

const natural = (value: string | undefined, fallback: number, max: number): number => {
  if (value === undefined) return fallback;
  const number = Number(value);
  if (!/^\d+$/.test(value) || !Number.isSafeInteger(number) || number > max) throw new Error("Invalid numeric option");
  return number;
};

async function watch(call: Call, id: string, start: number, until: boolean, expected?: string): Promise<void> {
  const target = object(await call({ op: "info", session: id, ...(expected ? { generation: expected } : {}) }));
  const session = target?.["session"];
  const generation = target?.["generation"];
  if (typeof session !== "string" || typeof generation !== "string") throw new Error("Invalid session identity");
  let cursor = start;
  for (;;) {
    const page = object(await call({ op: "events", session, generation, cursor }));
    const next = page?.["cursor"];
    const events = page?.["events"];
    if (!page || typeof next !== "number" || next < cursor || !Array.isArray(events))
      throw new Error("Invalid event page");
    if (page["gap"] === true) console.log(JSON.stringify({ type: "gap", session, generation, after: cursor }));
    for (const event of events) console.log(JSON.stringify({ ...object(event), session, generation }));
    if (until && events.some((event: unknown) => object(event)?.["type"] === "completion")) return;
    cursor = next;
    await Bun.sleep(1_000);
  }
}

export async function runRemote(args: string[]): Promise<number> {
  try {
    const { positionals, values } = parseArgs({
      args,
      allowPositionals: true,
      options: {
        help: { type: "boolean" },
        on: { type: "string" },
        via: { type: "string" },
        generation: { type: "string" },
        id: { type: "string" },
        "no-wait": { type: "boolean" },
        text: { type: "string" },
        steer: { type: "boolean" },
        limit: { type: "string" },
        cursor: { type: "string" },
        "until-completion": { type: "boolean" },
        model: { type: "string" },
        force: { type: "boolean" },
        observe: { type: "boolean" },
        cwd: { type: "string" },
        name: { type: "string" },
        thinking: { type: "string" },
        worktree: { type: "string" },
        branch: { type: "string" },
        base: { type: "string" },
      },
    });
    const [op, id, value] = positionals;
    if (values.help || op === undefined) {
      console.log(usage);
      return 0;
    }
    if (values.via && !values.on) throw new Error("--via requires --on");
    const argv = values.on ? peer(values.on, values.via ?? "dori") : undefined;
    // The querying machine and destination can have different installations and configuration.
    let deps: FlowDeps | undefined;
    if (argv === undefined) {
      const config = await loadConfig();
      deps = { config, clock: realClock, run, registry: new Registry(config.stateDir) };
    }
    if (op === "accept") {
      if (!deps || id !== undefined || Object.keys(values).length) throw new Error("Invalid target worker invocation");
      await acceptRemote(deps);
      return 0;
    }
    if (op === "policy") {
      if (!deps || Object.keys(values).length) throw new Error("Change incoming remote policy on the target itself");
      if (positionals.length > 3 || (id !== undefined && id !== "control" && id !== "wake"))
        throw new Error("Use policy [control|wake on|off]");
      if (id !== undefined && value !== "on" && value !== "off") throw new Error("Use policy [control|wake on|off]");
      console.log(
        JSON.stringify(
          id === undefined
            ? remotePolicy(deps.config.stateDir)
            : setRemotePolicy(deps.config.stateDir, id, value === "on"),
        ),
      );
      return 0;
    }
    const allowed = Object.hasOwn(flagsFor, op) ? flagsFor[op] : undefined;
    if (!allowed) throw new Error("Unknown remote operation; see dori remote --help");
    const unsupported = Object.keys(values).find(
      (flag) =>
        !["on", "via"].includes(flag) &&
        !(flag === "generation" && op !== "sessions" && op !== "open" && op !== "wake") &&
        !allowed.includes(flag),
    );
    if (unsupported) throw new Error(`--${unsupported} is not supported by remote ${op}`);
    const call: Call = argv
      ? (request) => throughPeer(argv, request)
      : (request) => {
          if (!deps) throw new Error("Missing local Cataly configuration");
          return remoteOperation(deps, request, false);
        };
    if (value !== undefined || positionals.length > 2 || (op === "open" || op === "sessions" ? id !== undefined : !id))
      throw new Error("Unexpected or missing session argument");
    const generation = values.generation;
    if (op === "watch") {
      if (!id) throw new Error("watch requires a session");
      await watch(
        call,
        id,
        natural(values.cursor, 0, Number.MAX_SAFE_INTEGER),
        values["until-completion"] ?? false,
        generation,
      );
      return 0;
    }
    if (op === "attach") {
      if (!id) throw new Error("attach requires a session");
      const result = object(await call({ op, session: id, ...(generation ? { generation } : {}) }));
      if (result?.["mode"] === "transcript") {
        if (typeof result["session"] !== "string" || typeof result["generation"] !== "string")
          throw new Error("No verified transcript to attach");
        const session = result["session"];
        const exact = result["generation"];
        console.log(JSON.stringify(await call({ op: "read", session, generation: exact })));
        await watch(call, session, natural(values.cursor, 0, Number.MAX_SAFE_INTEGER), false, exact);
        return 0;
      }
      const terminal = result?.["terminal"];
      const rootPid = result?.["rootPid"];
      if (typeof terminal !== "string" || !/^t[0-9a-z]+$/.test(terminal) || typeof rootPid !== "number")
        throw new Error("No verified terminal to attach");
      if (!process.stdin.isTTY || !process.stdout.isTTY) throw new Error("Attach needs an interactive local terminal");
      const address = values.on ? `${values.on}/${terminal}` : terminal;
      const confirmed = await run(["hc", "--json", "get", address], { timeoutMs: 10_000 });
      if (confirmed.code !== 0 || object(JSON.parse(confirmed.out))?.["pid"] !== rootPid)
        throw new Error("The local hc route does not resolve the verified terminal");
      const proc = Bun.spawn(["hc", "attach", ...(values.observe ? ["--observe"] : []), address], {
        stdin: "inherit",
        stdout: "inherit",
        stderr: "inherit",
      });
      return await proc.exited;
    }
    if (op === "send" && !values.text?.trim()) throw new Error("send requires non-empty --text");
    if (op === "receipt" && !values.id) throw new Error("receipt requires --id");
    if (op === "model" && !values.model) throw new Error("model requires --model");
    const { text, steer, force, model, cwd, name, thinking, worktree, branch, base } = values;
    // The peer wire omits unset fields, as does the local path.
    const request = Object.fromEntries(
      Object.entries({
        op,
        session: id,
        generation,
        id: op === "send" || op === "open" ? (values.id ?? randomUUID()) : values.id,
        text,
        steer,
        force,
        model,
        cwd,
        name,
        thinking,
        worktree,
        branch,
        base,
        limit: values.limit === undefined ? undefined : natural(values.limit, 20, 40),
      }).filter(([, item]) => item !== undefined),
    ) as Request;
    let result = await call(request);
    if (op === "send" && !values["no-wait"]) {
      const accepted = object(result);
      const session = accepted?.["session"];
      const exact = accepted?.["generation"];
      const requestId = request.id;
      if (typeof session === "string" && typeof exact === "string" && requestId) {
        for (let attempt = 0; attempt < 6 && object(result)?.["status"] === "pending"; attempt++) {
          await Bun.sleep(250);
          try {
            const receipt = object(await call({ op: "receipt", session, generation: exact, id: requestId }));
            if (!receipt || receipt["status"] === "not-found") break;
            result = { ...receipt, session, generation: exact };
          } catch {
            // The sender has an ID and generation to check; never send the text again after a lost reply.
            break;
          }
        }
      }
    }
    console.log(JSON.stringify(result));
    const outcome = object(result);
    const initial = object(outcome?.["delivery"]);
    if (outcome?.["status"] === "refused" || outcome?.["status"] === "not-found") return 2;
    if (outcome?.["deliveryError"] !== undefined || outcome?.["status"] === "close-unconfirmed") return 3;
    if (initial?.["status"] === "refused" || initial?.["status"] === "unconfirmed") return 3;
    if (op === "send" && !values["no-wait"] && outcome?.["status"] === "pending") return 3;
    return 0;
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    return 1;
  }
}
