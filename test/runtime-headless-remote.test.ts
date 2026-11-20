import { expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { shq } from "../src/delivery.ts";
import { HeadlessRuntime, headlessArgv } from "../src/runtime/headless.ts";

const fixtureHost = resolve(import.meta.dir, "fixtures/headless-rpc.ts");

test.skipIf(process.platform === "win32")(
  "remote native mutation shares the held lane's identity and receipt ledger",
  async () => {
    const dir = mkdtempSync("/tmp/cataly-headless-remote-");
    const omp = join(dir, "omp");
    const trace = join(dir, "trace.jsonl");
    const backgroundDone = join(dir, "background-done");
    writeFileSync(omp, `#!/bin/sh\nexec ${shq(process.execPath)} ${shq(fixtureHost)} "$@"\n`, { mode: 0o700 });
    const env = {
      HOME: dir,
      PATH: "/usr/bin:/bin",
      CATALY_TEST_SESSION: join(dir, "session.jsonl"),
      CATALY_TEST_TRACE: trace,
      CATALY_TEST_BACKGROUND_DONE: backgroundDone,
    };
    const rt = new HeadlessRuntime(dir, env);
    const ref = await rt.create({ role: "lane", container: "tab", cwd: dir, env, name: "remote" });
    let session: string | undefined;
    try {
      session = (await rt.startNative(ref, headlessArgv([omp, "{prompt}"], "fixture/initial"), env)).sessionId;
      if (!session) throw new Error("native lane ID missing");
      const id = session;
      const peer = new HeadlessRuntime(dir, env);
      const literal = "slow admission: $HOME $(false) `verbatim` 'exact'\nsecond line";
      const pending = rt.remoteSend(ref, id, { id: "send-one", text: literal });
      for (let i = 0; i < 100 && !(await peer.remoteReceipt(ref, id, "send-one")); i++) await Bun.sleep(5);
      expect(await peer.remoteReceipt(ref, id, "send-one")).toMatchObject({
        status: "pending",
        replyAttributed: false,
      });
      const receipt = await pending;
      expect(receipt).toEqual({
        id: "send-one",
        status: "admitted",
        receipt: { id: "send-one", kind: "native-submission" },
        replyAttributed: false,
      });
      expect(await peer.remoteSend(ref, id, { id: "send-one", text: literal })).toEqual(receipt);
      await expect(peer.remoteSend(ref, id, { id: "send-one", text: "changed" })).rejects.toThrow("different input");
      await expect(peer.remoteReceipt(ref, "unrelated-session", "send-one")).rejects.toThrow("identity mismatch");
      expect(await peer.remoteReceipt(ref, id, "missing")).toBeUndefined();
      expect(await rt.remoteSend(ref, id, { id: "slash", text: "/close" })).toMatchObject({ status: "refused" });

      expect(await peer.remoteModel(ref, id, "fixture/model/variant")).toEqual({
        status: "changed",
        model: "fixture/model/variant",
      });
      expect((await rt.observe(ref)).model).toBe("fixture/model/variant");
      await rt.begin(ref, id, "Verify remote native control");
      await rt.setGoal(ref, id, "paused");
      expect(await peer.remoteSend(ref, id, { id: "paused", text: "stay quiet" })).toMatchObject({ status: "refused" });
      await rt.setGoal(ref, id, "active");

      expect(await peer.remoteSend(ref, id, { id: "ask", text: "request question" })).toMatchObject({
        status: "admitted",
      });
      const question = (await peer.observe(ref)).dialogs[0];
      if (!question) throw new Error("native question did not arrive");
      expect(await peer.remoteClose(ref, id, true)).toMatchObject({ status: "refused" });
      expect(await peer.remoteInterrupt(ref, id)).toMatchObject({ status: "refused", background: "unverified" });
      expect(await peer.remoteModel(ref, id, "fixture/held")).toMatchObject({ status: "refused" });
      expect(await peer.remoteSend(ref, id, { id: "held", text: "do not admit", delivery: "steer" })).toMatchObject({
        status: "refused",
      });
      await rt.answer(ref, question.id, { cancelled: true }, id);

      expect(await peer.remoteSend(ref, id, { id: "foreground", text: "long turn" })).toMatchObject({
        status: "admitted",
      });
      expect(await peer.remoteClose(ref, id)).toMatchObject({ status: "refused" });
      expect(await peer.remoteClose(ref, id, true)).toMatchObject({ status: "refused" });
      expect(await peer.remoteSend(ref, id, { id: "idle-only", text: "do not queue" })).toMatchObject({
        status: "refused",
      });
      expect(await peer.remoteSend(ref, id, { id: "steered", text: "steer this", delivery: "steer" })).toMatchObject({
        status: "admitted",
      });
      expect(await peer.remoteInterrupt(ref, id)).toEqual({
        status: "interrupted",
        foreground: true,
        background: "unverified",
      });

      await peer.remoteSend(ref, id, { id: "background", text: "background job" });
      expect(await peer.remoteInterrupt(ref, id)).toMatchObject({
        status: "refused",
        foreground: false,
        background: "unverified",
      });
      expect(await peer.remoteClose(ref, id)).toMatchObject({ status: "refused" });
      expect(await peer.remoteClose(ref, id, true)).toMatchObject({ status: "refused" });
      writeFileSync(backgroundDone, "");
      expect(await peer.remoteClose(ref, id, true)).toEqual({ status: "closed" });
      expect((await rt.observe(ref)).status).toBe("gone");
      expect(await peer.remoteClose(ref, id, true)).toEqual({ status: "closed" });
      const frames = readFileSync(trace, "utf8")
        .trim()
        .split("\n")
        .map((line) => JSON.parse(line) as Record<string, unknown>);
      const input = frames.filter((f) => f["type"] === "follow_up" || f["type"] === "steer");
      expect(input.map((f) => [f["type"], f["message"]])).toEqual([
        ["follow_up", literal],
        ["follow_up", "request question"],
        ["follow_up", "long turn"],
        ["steer", "steer this"],
        ["follow_up", "background job"],
      ]);
      expect(frames.filter((f) => f["type"] === "abort")).toHaveLength(1);
    } finally {
      writeFileSync(backgroundDone, "");
      const live = await rt.observe(ref);
      if (session && live.status !== "gone") {
        for (const dialog of live.dialogs) await rt.answer(ref, dialog.id, { cancelled: true }, session);
        await rt.remoteClose(ref, session, true);
      }
      rmSync(dir, { recursive: true, force: true });
    }
  },
  25_000,
);
