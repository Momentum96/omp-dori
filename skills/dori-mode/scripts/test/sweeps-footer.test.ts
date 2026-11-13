import { afterEach, beforeEach, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { adoptLane, LaunchError } from "../src/launch.ts";
import { cliCommand, footer, shellQuote } from "../src/launch-footer.ts";
import type { Lane } from "../src/registry.ts";
import { depsFor, fakeClock, newWorld, withState, type World } from "./fakes.ts";

const T0 = Date.parse("2026-01-01T12:00:00.000Z");

let state: ReturnType<typeof withState>;

let world: World;

beforeEach(() => {
  state = withState();
  world = newWorld();
});

afterEach(() => state.done());

const working: Lane = {
  key: "quiet-lane",
  title: "quiet",
  thread: "chat:team/9",
  pane: "w:p7",
  brief: "/b.md",
  done: "merged acme/app#1",
  openedAt: new Date(T0).toISOString(),
  lastReplyAt: T0,
};

test("the lane footer calls the skill CLI by its own path, never a bare dori that may be the root harness", async () => {
  const text = footer({ ...working, key: "path-lane" }, "lead:p1");
  const commands = [...text.matchAll(/bun (\S+) (report|claim-done) path-lane/g)];
  expect(commands.map((m) => m[2])).toEqual(["report", "report", "claim-done"]);
  for (const [, cli] of commands) {
    expect(cli).toEndWith("/skills/dori-mode/scripts/src/cli.ts");
    expect(await Bun.file(cli ?? "").exists()).toBe(true);
  }
  expect(text).not.toMatch(/(^|\s)dori (report|claim-done)/);
});

test("adopting a lane refuses a Done line the watcher could never check", async () => {
  const deps = depsFor(world, fakeClock(T0), state.dir);
  await expect(
    adoptLane(deps, { key: "x-lane", title: "x", brief: "/b.md", done: "two clean QA passes", pane: "w:p2" }),
  ).rejects.toBeInstanceOf(LaunchError);
  const ok = await adoptLane(deps, {
    key: "x-lane",
    title: "x",
    brief: "/b.md",
    done: "closed acme/app#3",
    pane: "w:p2",
  });
  expect(ok.thread).toBe("none");
  await expect(
    adoptLane(deps, { key: "x-lane", title: "x", brief: "/b.md", done: "closed acme/app#3", pane: "w:p2" }),
  ).rejects.toThrow("already registered");
});

test("the footer CLI command survives a skill path with a space or a quote", async () => {
  const dir = mkdtempSync(join(tmpdir(), "dori skill's dir "));
  try {
    const cli = join(dir, "cli.ts");
    writeFileSync(cli, 'console.log("CLI_OK " + process.argv.slice(2).join(" "));\n');
    expect(shellQuote("/plain/path/cli.ts")).toBe("/plain/path/cli.ts");
    const command = cliCommand(cli);
    const proc = Bun.spawn(["bash", "-c", `${command} report path-lane --kind milestone`], {
      stdout: "pipe",
      stderr: "pipe",
    });
    expect((await new Response(proc.stdout).text()).trim()).toBe("CLI_OK report path-lane --kind milestone");
    expect(await proc.exited).toBe(0);
    expect(footer({ ...working, key: "path-lane" }, "lead:p1")).toContain(cliCommand());
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the footer's report command sends the [REPORT] line to the lead as one argument on each runtime", async () => {
  const dir = mkdtempSync(join(tmpdir(), "dori footer send "));
  try {
    const cli = join(dir, "cli.ts");
    writeFileSync(
      cli,
      'const [, , , key, , kind, , text] = process.argv;\nconsole.log(["[REPORT]", key, "|", kind, "|", text].join(" "));\n',
    );
    for (const bin of ["herdr", "hc"])
      writeFileSync(join(dir, bin), `#!/bin/sh\nprintf '%s\\n' ${bin} "$@" > "${dir}/${bin}.argv"\n`, {
        mode: 0o755,
      });
    for (const [runtime, sent] of [
      ["herdr", ["herdr", "pane", "run", "lead:p1"]],
      ["hc", ["hc", "send", "lead:p1", "--"]],
    ] as const) {
      const line = footer({ ...working, key: "send-lane" }, "lead:p1", {}, runtime)
        .split("\n")
        .find((l) => l.startsWith("- Persist"));
      const command = (line?.split("never spliced into a command string: ")[1] ?? "")
        .replaceAll(cliCommand(), cliCommand(cli))
        .replace("<kind>", "milestone")
        .replace("<text>", "it's done; ok | 100%");
      const proc = Bun.spawn(["bash", "-c", command], {
        stdout: "pipe",
        env: { ...process.env, PATH: `${dir}:${process.env["PATH"]}` },
      });
      const report = "[REPORT] send-lane | milestone | it's done; ok | 100%";
      expect((await new Response(proc.stdout).text()).trim()).toBe(report);
      expect(await proc.exited).toBe(0);
      expect((await Bun.file(join(dir, `${runtime}.argv`)).text()).split("\n").slice(0, -1)).toEqual([...sent, report]);
    }
    expect(footer(working, "lead:p1")).toContain('herdr pane run lead:p1 "$line" (on hc: hc send lead:p1 -- "$line")');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
