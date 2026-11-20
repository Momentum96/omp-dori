import { expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { shq } from "../src/delivery.ts";
import { remoteSocketDir } from "../src/remote/socket-path.ts";
import { observeRemotePty } from "./fixtures/remote-attach.ts";
import { remoteTestModel } from "./fixtures/remote-test-model.ts";

const hc = Bun.which("hc");
const host = process.env["DORI_TEST_OMP_ROOT"];
const cli = resolve(import.meta.dir, "../skills/dori-mode/scripts/src/cli.ts");
const extension = resolve(import.meta.dir, "../extension/index.ts");
const pinned = process.env["DORI_TEST_OMP_EXEC"];
const ompArgv = pinned ? [pinned] : host ? [process.execPath, join(host, "dist", "cli.js")] : undefined;

type Env = Record<string, string | undefined>;
async function run(env: Env, argv: string[]) {
  const child = Bun.spawn(argv, { env, stdout: "pipe", stderr: "pipe", timeout: 35_000 });
  const [out, err, code] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  if (code) throw new Error(`${argv.slice(0, 3).join(" ")} -> ${code}: ${err || out}`);
  return out.trim();
}

async function waitFor<T>(fn: () => Promise<T | undefined>, message: string): Promise<T> {
  const expires = Date.now() + 18_000;
  do {
    const result = await fn();
    if (result !== undefined) return result;
    await Bun.sleep(150);
  } while (Date.now() < expires);
  throw new Error(message);
}

// Uses two actual hc cores over hc's normal peer protocol on one host, with a real pinned OMP TUI and local SSE.
test.skipIf(process.platform === "win32" || !hc || !ompArgv)(
  "a peer discovers, prompts, reads, changes model, and closes a real OMP session",
  async () => {
    const root = mkdtempSync("/tmp/cataly-remote-e2e-");
    const scope = join(root, "state");
    const vars = (side: "a" | "b"): Env => ({
      PATH: `${Bun.env["PATH"] ?? "/usr/bin:/bin"}:${resolve(process.execPath, "..")}`,
      HOME: root,
      XDG_CONFIG_HOME: join(root, side, "c"),
      XDG_STATE_HOME: join(root, side, "s"),
      XDG_RUNTIME_DIR: join(root, side, "r"),
      HC_SESSION: `remote-${side}`,
      TERM: "xterm-256color",
      SHELL: "/bin/sh",
      DORI_STATE_DIR: side === "b" ? scope : join(root, "source-state"),
      DORI_CONFIG: join(root, "config.json"),
      PI_CODING_AGENT_DIR: join(root, "omp"),
      NO_PROXY: "127.0.0.1",
    });
    const a = vars("a");
    const b = vars("b");
    const dir = join(root, "omp");
    const model = "remote-fixture";
    const prompt = "native peer says: $HOME $(false) 'exact'";
    const firstPrompt = "First prompt from remote open";
    const response = "REAL-NATIVE-REPLY";
    const fixture = remoteTestModel(dir, model, [prompt, firstPrompt], response);
    const failedCleanup: unknown[] = [];
    try {
      for (const env of [a, b]) {
        for (const k of ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"])
          mkdirSync(env[k]!, { recursive: true, mode: 0o700 });
        mkdirSync(join(env["XDG_CONFIG_HOME"]!, "herdr-core"), { recursive: true });
        writeFileSync(join(env["XDG_CONFIG_HOME"]!, "herdr-core", "config.toml"), "notes = false\n");
        chmodSync(env["XDG_RUNTIME_DIR"]!, 0o700);
      }
      writeFileSync(
        join(root, "config.json"),
        JSON.stringify({
          ompUlwPath: "",
          stateDir: scope,
          sessionsDir: join(dir, "sessions"),
          agentCommand: [
            ...ompArgv!,
            "--no-extensions",
            "-e",
            extension,
            "--dori-role",
            "lane",
            "--model",
            "{model}",
            "{prompt}",
          ],
        }),
      );
      const worker = join(root, "dori-peer");
      writeFileSync(worker, `#!/bin/sh\nexec ${shq(process.execPath)} ${shq(cli)} "$@"\n`, { mode: 0o700 });
      const invoke = (env: Env, ...args: string[]) => run(env, [process.execPath, cli, "remote", ...args]);
      await run(b, [hc!, "server", "start"]);
      await run(a, [hc!, "server", "start"]);
      // hc injects the remote shell command as the last argument; all scope variables are explicit.
      const wire = `env ${Object.entries(b)
        .map(([key, value]) => shq(`${key}=${value}`))
        .join(" ")} /bin/sh -c`;
      await run(a, [hc!, "machine", "add", "--name", "test-peer", "--via", wire, "--session", b["HC_SESSION"]!]);
      await invoke(b, "policy", "control", "on");
      const remote = (...args: string[]) => invoke(a, ...args, "--on", "test-peer", "--via", worker);
      const repository = join(root, "repo");
      const worktree = join(root, "worktree");
      mkdirSync(repository);
      await run(b, ["git", "-C", repository, "init", "-q", "--initial-branch=main"]);
      await run(b, [
        "git",
        "-C",
        repository,
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--allow-empty",
        "-m",
        "Fixture",
      ]);
      const open = [
        "open",
        "--cwd",
        repository,
        "--worktree",
        worktree,
        "--branch",
        "remote-task",
        "--base",
        "main",
        "--model",
        `cataly-local/${model}`,
        "--thinking",
        "off",
        "--id",
        "open-once",
        "--text",
        firstPrompt,
      ];
      const opened = await remote(...open).catch(async (error: unknown) => {
        const terminal = await run(b, [hc!, "--json", "get", "t1"]).catch(String);
        const screen = await run(b, [hc!, "--json", "read", "t1", "--lines", "18"]).catch(String);
        const session = await invoke(b, "sessions").catch(String);
        throw new Error(`${String(error)}\nterminal=${terminal}\nscreen=${screen}\nsessions=${session}`);
      });
      expect(JSON.parse(opened)).toMatchObject({ status: "opened", model: `cataly-local/${model}`, worktree });
      expect(await remote(...open)).toBe(opened);
      expect(await run(b, ["git", "-C", worktree, "branch", "--show-current"])).toBe("remote-task");
      const target = await waitFor(async () => {
        const rows = JSON.parse(await remote("sessions")) as Array<{
          session: string;
          generation: string;
          status: string;
          pid: number;
        }>;
        return rows.find((row) => row.status === "idle");
      }, "real OMP TUI did not publish a native endpoint");
      expect(JSON.parse(await remote("read", target.session)).messages).toEqual(
        expect.arrayContaining([expect.objectContaining({ role: "user", text: firstPrompt })]),
      );
      const send = JSON.parse(await remote("send", target.session, "--text", prompt, "--id", "native-e2e"));
      expect(send).toMatchObject({ status: "admitted", session: target.session });
      const messages = await waitFor(async () => {
        const result = JSON.parse(await remote("read", target.session)) as {
          messages: Array<{ text: string; role: string }>;
        };
        return result.messages.filter((entry) => entry.role === "assistant" && entry.text === response).length >= 2
          ? result.messages
          : undefined;
      }, "real OMP reply did not arrive");
      const replay = JSON.parse(
        await remote("send", target.session, "--text", prompt, "--id", "native-e2e", "--generation", target.generation),
      );
      expect(replay).toMatchObject({ status: "admitted", session: target.session });
      expect(messages.filter((entry) => entry.role === "user" && entry.text === prompt)).toHaveLength(1);
      expect(JSON.parse(await remote("read", target.session)).messages).toEqual(messages);
      // Auxiliary OMP requests (for example title generation) may also include the text.
      expect(fixture.submissions()).toBeGreaterThan(0);
      const events = await remote("watch", target.session, "--until-completion", "--generation", target.generation);
      expect(events.split("\n").map((line) => JSON.parse(line))).toEqual(
        expect.arrayContaining([expect.objectContaining({ type: "completion" })]),
      );
      await observeRemotePty(
        [process.execPath, cli, "remote", "attach", target.session, "--on", "test-peer", "--via", worker, "--observe"],
        a,
        response,
      );
      expect(
        JSON.parse(await remote("model", target.session, "--model", "cataly-local/alternate-fixture")),
      ).toMatchObject({ model: "cataly-local/alternate-fixture" });
      expect(JSON.parse(await remote("interrupt", target.session))).toMatchObject({ status: "idle" });
      await invoke(b, "policy", "wake", "on");
      process.kill(target.pid, "SIGKILL");
      await waitFor(
        async () => (JSON.parse(await remote("sessions")).length === 0 ? true : undefined),
        "old OMP endpoint still live",
      );
      const woke = JSON.parse(await remote("wake", target.session));
      expect(woke).toMatchObject({ session: target.session, status: "idle" });
      expect(woke.generation).not.toBe(target.generation);
      expect(JSON.parse(await remote("close", target.session))).toMatchObject({ status: "closed" });
      expect(JSON.parse(await remote("sessions"))).toEqual([]);
      await expect(remote("wake", target.session)).rejects.toThrow("Explicitly closed");
    } finally {
      for (const env of [a, b]) {
        try {
          await run(env, [hc!, "server", "stop"]);
        } catch (error) {
          failedCleanup.push(error);
        }
      }
      await fixture.server.stop(true);
      rmSync(remoteSocketDir(scope), { force: true, recursive: true });
      rmSync(root, { force: true, recursive: true });
      expect(failedCleanup).toEqual([]);
    }
  },
  100_000,
);
