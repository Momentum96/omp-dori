import { parseArgs } from "node:util";
import { HeadlessRuntime, type HeadlessAnswer } from "../../../../src/runtime/headless.ts";
import { object } from "../../../../src/remote/client.ts";
import { loadConfig } from "./config.ts";
import { LANE_KEY, Registry, sessionOf, statusOf } from "./registry.ts";
import { runtimesFor } from "./run.ts";

function answerOf(value: unknown): HeadlessAnswer {
  const record = object(value);
  if (!record || Object.keys(record).length !== 1) throw new Error("Expected one native answer field");
  if (record["cancelled"] === true) return { cancelled: true };
  if (typeof record["confirmed"] === "boolean") return { confirmed: record["confirmed"] };
  if (typeof record["value"] === "string") return { value: record["value"] };
  const answers = record["answers"];
  if (!Array.isArray(answers)) throw new Error("Expected cancelled, confirmed, value or answers");
  const selected = answers.map((answer: unknown) => {
    const question = object(answer);
    const id = question?.["id"];
    const options = question?.["selectedOptions"];
    const custom = question?.["customInput"];
    if (typeof id !== "string" || !Array.isArray(options) || !options.every((x) => typeof x === "string"))
      throw new Error("Invalid native question answer");
    if (custom !== undefined && typeof custom !== "string") throw new Error("Invalid native custom input");
    return { id, selectedOptions: options as string[], ...(custom === undefined ? {} : { customInput: custom }) };
  });
  return { answers: selected };
}

async function readAnswer(path: string): Promise<HeadlessAnswer> {
  const source = path === "-" ? Bun.stdin.stream() : Bun.file(path).stream();
  let bytes = 0;
  const chunks: Uint8Array[] = [];
  for await (const chunk of source) {
    bytes += chunk.byteLength;
    if (bytes > 64 * 1024) throw new Error("Native answer is too large");
    chunks.push(chunk);
  }
  let value: unknown;
  try {
    value = JSON.parse(Buffer.concat(chunks).toString());
  } catch {
    throw new Error("Native answer must be JSON");
  }
  return answerOf(value);
}

export async function runHeadless(args: string[]): Promise<number> {
  try {
    const { positionals, values } = parseArgs({
      args,
      allowPositionals: true,
      options: { help: { type: "boolean" }, "answer-file": { type: "string" } },
    });
    if (values.help || positionals.length === 0) {
      console.log("dori headless observe LANE | answer LANE REQUEST_ID --answer-file FILE|-");
      return 0;
    }
    const [op, key, id] = positionals;
    if (
      !key ||
      !LANE_KEY.test(key) ||
      (!(op === "observe" && positionals.length === 2 && !values["answer-file"]) &&
        !(op === "answer" && positionals.length === 3 && id && values["answer-file"]))
    )
      throw new Error("See dori headless --help");
    const config = await loadConfig();
    const registry = new Registry(config.stateDir);
    const lane = await registry.read(key);
    const ref = lane && sessionOf(lane);
    if (!lane || !ref || ref.runtime !== "headless") throw new Error("No registered headless lane by that name");
    const runtime = runtimesFor(config, process.env).for(ref);
    if (!(runtime instanceof HeadlessRuntime)) throw new Error("No headless runtime configured");
    if (op === "observe") {
      console.log(JSON.stringify(await runtime.observe(ref)));
      return 0;
    }
    if (!id || !values["answer-file"] || !lane.session || statusOf(lane) === "closed" || lane.nativeCloseIntent)
      throw new Error("No open, bound native session to answer");
    const answer = await readAnswer(values["answer-file"]);
    const fresh = await registry.read(key);
    if (
      !fresh ||
      statusOf(fresh) === "closed" ||
      fresh.nativeCloseIntent ||
      fresh.session !== lane.session ||
      fresh.lifecycleEpoch !== lane.lifecycleEpoch ||
      sessionOf(fresh)?.id !== ref.id
    )
      throw new Error("Lane ownership changed while reading the answer");
    console.log(JSON.stringify(await runtime.answer(ref, id, answer, lane.session)));
    return 0;
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    return 1;
  }
}
