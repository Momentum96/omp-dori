import { afterEach, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { liveSessions, selectSession } from "../src/remote/client.ts";
import { remotePolicy, setRemotePolicy } from "../src/remote/policy.ts";
import { remoteSocketDir, remoteSocketPath } from "../src/remote/socket-path.ts";
import { peer, throughPeer } from "../src/remote/transport.ts";

const CLI = resolve(import.meta.dir, "../skills/dori-mode/scripts/src/cli.ts");
const cleanup: Array<() => void | Promise<void>> = [];
afterEach(async () => {
  for (const dispose of cleanup.splice(0).reverse()) await dispose();
});

function sandbox() {
  const root = mkdtempSync(join(tmpdir(), "cataly-remote-test-"));
  const state = join(root, "state");
  cleanup.push(() => rmSync(root, { force: true, recursive: true }));
  const env = { ...process.env, DORI_CONFIG: join(root, "missing-config.json"), DORI_STATE_DIR: state };
  return { root, state, env };
}

async function endpoint(state: string, session = "native-001") {
  const dir = remoteSocketDir(state);
  mkdirSync(dir, { mode: 0o700, recursive: true });
  cleanup.push(() => rmSync(dir, { force: true, recursive: true }));
  const path = remoteSocketPath(state, session);
  const target = { session, generation: "original", role: "lane", status: "idle", cwd: "/project" } as const;
  let inputs = 0;
  let closeReply: "refused" | "lost" = "refused";
  const server = createServer((connection) => {
    connection.setEncoding("utf8");
    let pending = "";
    connection.on("data", (bytes: string) => {
      pending += bytes;
      if (!pending.includes("\n")) return;
      const request = JSON.parse(pending) as { op: string; generation?: string; text?: string };
      if (request.op === "info") connection.end(`${JSON.stringify({ ok: true, data: target })}\n`);
      else if (request.generation !== target.generation) connection.end('{"ok":false,"error":"wrong generation"}\n');
      else if (request.op === "read")
        connection.end('{"ok":true,"data":{"messages":[{"role":"assistant","text":"status"}]}}\n');
      else if (request.op === "events")
        connection.end('{"ok":true,"data":{"cursor":1,"events":[{"cursor":1,"type":"completion"}],"gap":false}}\n');
      else if (request.op === "send") {
        inputs++;
        connection.end(`${JSON.stringify({ ok: true, data: { status: "admitted", text: request.text } })}\n`);
      } else if (request.op === "close") {
        if (closeReply === "lost") connection.end();
        else connection.end('{"ok":false,"error":"close held by approval"}\n');
      } else connection.end('{"ok":false,"error":"unsupported"}\n');
    });
  });
  await new Promise<Server>((done, reject) => {
    server.once("error", reject);
    server.listen(path, () => done(server));
  });
  cleanup.push(() => new Promise<void>((done) => server.close(() => done())));
  chmodSync(path, 0o600);
  return { target, inputs: () => inputs, loseCloseReply: () => (closeReply = "lost") };
}

test.skipIf(process.platform === "win32")(
  "target CLI checks opt-in policy per request and never treats input as argv",
  async () => {
    const { state, env } = sandbox();
    const session = await endpoint(state);
    const text = "'\" $HOME $(echo no)\nsecond line";
    const accept = async (request: object) => {
      const p = Bun.spawn([process.execPath, CLI, "remote", "accept"], {
        env,
        stdin: new Blob([JSON.stringify(request)]),
        stdout: "pipe",
        stderr: "pipe",
      });
      const [out, err, code] = await Promise.all([
        new Response(p.stdout).text(),
        new Response(p.stderr).text(),
        p.exited,
      ]);
      expect({ err, code }).toEqual({ err: "", code: 0 });
      return JSON.parse(out);
    };
    expect(await accept({ op: "sessions" })).toMatchObject({ ok: false, error: expect.stringContaining("disabled") });
    expect(remotePolicy(state)).toEqual({ control: false, wake: false });
    setRemotePolicy(state, "control", true);
    expect(await accept({ op: "sessions" })).toMatchObject({ ok: true, data: [session.target] });
    expect(await accept({ op: "read", session: "native" })).toMatchObject({
      ok: true,
      data: { messages: [{ role: "assistant", text: "status" }] },
    });
    expect(await accept({ op: "send", session: "native-001", generation: "stale", text })).toMatchObject({ ok: false });
    expect(await accept({ op: "send", session: "native-001", generation: "original", id: "once", text })).toMatchObject(
      {
        ok: true,
        data: { status: "admitted", text },
      },
    );
    expect(session.inputs()).toBe(1);
    expect(await accept({ op: "wake", session: "native-001" })).toMatchObject({
      ok: false,
      error: expect.stringContaining("wake is disabled"),
    });
    setRemotePolicy(state, "wake", true);
    expect(await accept({ op: "close", session: "native-001" })).toEqual({
      ok: false,
      error: "close held by approval",
    });
    expect(await accept({ op: "wake", session: "native-001" })).toMatchObject({ ok: true, data: session.target });
    session.loseCloseReply();
    expect(await accept({ op: "close", session: "native-001" })).toMatchObject({
      ok: false,
      error: expect.stringContaining("receipt"),
    });
    expect(await accept({ op: "wake", session: "native-001" })).toMatchObject({
      ok: false,
      error: expect.stringContaining("close outcome is unconfirmed"),
    });
    setRemotePolicy(state, "control", false);
    expect(await accept({ op: "read", session: "native-001" })).toMatchObject({ ok: false });
  },
);

test.skipIf(process.platform === "win32")(
  "live discovery rejects unsafe sockets and selection rejects ambiguous IDs",
  async () => {
    const { state } = sandbox();
    const { target } = await endpoint(state);
    expect(await liveSessions(state)).toEqual([target]);
    expect(() => selectSession([target, { ...target, session: "native-002" }], "native")).toThrow("Ambiguous");
    chmodSync(remoteSocketPath(state, target.session), 0o666);
    expect(await liveSessions(state)).toEqual([]);
  },
);

test("remote request transport keeps text out of process arguments", async () => {
  const argv = peer("lab-peer", "/separate home/bin/dori");
  expect(argv).toEqual([
    "hc",
    "exec",
    "--on",
    "lab-peer",
    "--timeout",
    "55",
    "--stdin",
    "--",
    "/separate home/bin/dori",
    "remote",
    "accept",
  ]);
  expect(() => peer("-invalid", "dori")).toThrow("Invalid");
  await expect(
    throughPeer([process.execPath, "-e", "console.log(JSON.stringify({ok:true,data:await Bun.stdin.json()}))"], {
      op: "send",
      text: "source\nline",
      id: "request-1",
    }),
  ).resolves.toEqual({ op: "send", text: "source\nline", id: "request-1" });
});

test.skipIf(process.platform === "win32")("untrusted policy mode never silently enables access", () => {
  const { state } = sandbox();
  mkdirSync(state, { recursive: true });
  const path = join(state, "remote-policy.json");
  writeFileSync(path, '{"control":true,"wake":true}', { mode: 0o644 });
  expect(() => remotePolicy(state)).toThrow("owner-only");
});
