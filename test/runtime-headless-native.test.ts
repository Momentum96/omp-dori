import { expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { Registry } from "../skills/dori-mode/scripts/src/registry.ts";
import { HeadlessRuntime, type HeadlessObservation } from "../src/runtime/headless.ts";
import type { SessionRef } from "../src/runtime/types.ts";

const native = process.env["DORI_TEST_OMP_ROOT"];
const runSmoke = process.env["CATALY_HEADLESS_SMOKE"] === "1" && process.platform !== "win32";
const cli = resolve(import.meta.dir, "../skills/dori-mode/scripts/src/cli.ts");

async function runCli(env: Record<string, string>, ...args: string[]): Promise<string> {
  const p = Bun.spawn([process.execPath, cli, ...args], { env, stdout: "pipe", stderr: "pipe", timeout: 35_000 });
  const [out, err, code] = await Promise.all([new Response(p.stdout).text(), new Response(p.stderr).text(), p.exited]);
  if (code !== 0) throw new Error(`dori ${args[0]} exited ${code}: ${err || out}`);
  return out.trim();
}

// Exact, unmocked OMP and the Dori extension; only the model provider is a localhost fixture.
test.skipIf(!runSmoke)(
  "CLI launches, observes, reports, pauses and closes a pinned native headless lane",
  async () => {
    if (!native) throw new Error("DORI_TEST_OMP_ROOT must point to the pinned OMP dependency");
    const home = realpathSync(mkdtempSync("/tmp/cataly-headless-native-"));
    const cwd = join(home, "work");
    const stateDir = join(home, "state");
    const ompDir = join(home, "omp");
    mkdirSync(cwd);
    mkdirSync(ompDir);
    const config = join(home, "config.json");
    const key = "native-smoke";
    let submissions = 0;
    const marker = "CATALY_HEADLESS_LOCAL_GOAL";
    const reply = "CATALY_HEADLESS_LOCAL_ANSWER";
    const remoteText = "Headless remote literal: $HOME $(false) 'exact'\nsecond line";
    const remoteReply = "CATALY_HEADLESS_EXACT_REMOTE_ACCEPTED";
    const brief = join(cwd, `${marker}.txt`);
    const evidence = join(cwd, "evidence.txt");
    writeFileSync(brief, "A no-side-effects localhost smoke test.");
    writeFileSync(evidence, "smoke done signal");
    writeFileSync(
      config,
      JSON.stringify({
        runtime: "hc",
        stateDir,
        defaultCwd: cwd,
        sessionsDir: join(ompDir, "sessions"),
        ompUlwPath: "",
        defaultModel: "cataly-local/headless-fixture",
        agentCommand: [
          process.execPath,
          join(native, "dist", "cli.js"),
          "--no-extensions",
          "-e",
          resolve(import.meta.dir, "../extension/index.ts"),
          "--model",
          "{model}",
          "--thinking",
          "off",
          "{prompt}",
        ],
      }),
    );
    const provider = Bun.serve({
      hostname: "127.0.0.1",
      port: 0,
      async fetch(request) {
        const body = await request.text();
        const exactRemote = body.includes(JSON.stringify(remoteText).slice(1, -1));
        if (!body.includes(marker) && !exactRemote)
          return new Response("unexpected local fixture input", { status: 400 });
        submissions++;
        const frame = {
          id: "headless-local-completion",
          object: "chat.completion.chunk",
          created: 1,
          model: "headless-fixture",
          choices: [
            {
              index: 0,
              delta: { role: "assistant", content: exactRemote ? remoteReply : reply },
              finish_reason: "stop",
            },
          ],
        };
        return new Response(`data: ${JSON.stringify(frame)}\n\ndata: [DONE]\n\n`, {
          headers: { "content-type": "text/event-stream" },
        });
      },
    });
    writeFileSync(
      join(ompDir, "models.yml"),
      JSON.stringify({
        providers: {
          "cataly-local": {
            baseUrl: `${provider.url.origin}/v1`,
            apiKey: "local-fixture-only",
            api: "openai-completions",
            models: [
              { id: "headless-fixture", name: "Headless loopback fixture" },
              { id: "alternate", name: "Alternate loopback fixture" },
            ],
          },
        },
      }),
    );
    const env = {
      HOME: home,
      PATH: `/usr/bin:/bin:${dirname(process.execPath)}`,
      XDG_CONFIG_HOME: join(home, "config"),
      XDG_STATE_HOME: join(home, "xdg-state"),
      XDG_CACHE_HOME: join(home, "cache"),
      PI_CODING_AGENT_DIR: ompDir,
      DORI_HOME: home,
      DORI_CONFIG: config,
      DORI_STATE_DIR: stateDir,
      NO_PROXY: "127.0.0.1",
    };
    const rt = new HeadlessRuntime(stateDir, env);
    const registry = new Registry(stateDir);
    let ref: SessionRef | undefined;
    let sessionId: string | undefined;
    try {
      const launched = await runCli(
        env,
        "launch",
        key,
        "--runtime",
        "headless",
        "--title",
        "Live test",
        "--brief",
        brief,
        "--done",
        `file ${evidence}`,
      );
      expect(launched).toInclude(`STARTUP_OK ${key} headless:`);
      const lane = await registry.read(key);
      ref = lane?.sessionRef;
      sessionId = lane?.session;
      if (!ref || !sessionId) throw new Error("native startup did not attest the registered session");
      const observe = async () => JSON.parse(await runCli(env, "headless", "observe", key)) as HeadlessObservation;
      const waitForReply = async (expected: string) => {
        const deadline = Date.now() + 12_000;
        let observed = await observe();
        while (
          (!observed.recent.some((entry) => entry.text === expected) || observed.status !== "idle") &&
          Date.now() < deadline
        ) {
          await Bun.sleep(100);
          observed = await observe();
        }
        expect(observed.recent).toContainEqual({ kind: "assistant", text: expected });
        return observed;
      };
      const observed = await waitForReply(reply);
      expect(observed).toMatchObject({ role: "lane", sessionId, goal: { status: "active" }, held: false });
      expect(submissions).toBeGreaterThan(0);
      expect(JSON.parse(await runCli(env, "remote", "sessions"))).toEqual(
        expect.arrayContaining([
          expect.objectContaining({ session: sessionId, pane: `headless:${ref.id}`, lane: key }),
        ]),
      );
      expect(
        JSON.parse(await runCli(env, "remote", "model", sessionId, "--model", "cataly-local/alternate")),
      ).toMatchObject({ status: "changed", model: "cataly-local/alternate", session: sessionId });
      const remoteArgs = ["remote", "send", sessionId, "--id", "native-smoke-send", "--text", remoteText];
      const admitted = JSON.parse(await runCli(env, ...remoteArgs));
      expect(admitted).toMatchObject({ status: "admitted", replyAttributed: false, session: sessionId });
      expect(JSON.parse(await runCli(env, ...remoteArgs))).toEqual(admitted);
      expect(JSON.parse(await runCli(env, "remote", "receipt", sessionId, "--id", "native-smoke-send"))).toMatchObject({
        status: "admitted",
        id: "native-smoke-send",
        replyAttributed: false,
      });
      expect((await waitForReply(remoteReply)).model).toBe("cataly-local/alternate");
      const report = JSON.parse(
        await runCli(env, "report", key, "--kind", "milestone", "--text", "verified localhost", "--json"),
      );
      expect(report).toMatchObject({ generation: ref.id, text: "verified localhost" });
      expect(await runCli(env, "pause", key)).toStartWith(`PAUSED ${key}`);
      expect(await observe()).toMatchObject({
        sessionId,
        status: "idle",
        goal: { status: "paused" },
        held: false,
      });
      expect(await rt.deliver(ref, "must not wake a paused lane")).toMatchObject({ kind: "not-ready" });
      expect(await runCli(env, "close", key)).toInclude(`CLOSED ${key}`);
      expect(await observe()).toMatchObject({ status: "gone" });
    } finally {
      try {
        if (!ref || !sessionId) {
          const lane = await registry.read(key);
          ref = lane?.sessionRef;
          sessionId = lane?.session;
        }
        if (ref && sessionId && (await rt.observe(ref)).status !== "gone") await rt.remoteClose(ref, sessionId, true);
      } finally {
        await provider.stop(true);
        rmSync(home, { force: true, recursive: true });
      }
    }
  },
  40_000,
);
