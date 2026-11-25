// Bridge authority tests (DESIGN 8, EXT req. 9): a subagent's shell veto and the private socket directory.
// The module is imported whole so that each test fails on its own against a bridge without these parts.
import { expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as cataly from "./cataly";
import { ctxOf, fakePi, laneCtx, laneHub } from "./fakes";

/** A subagent's context as OMP 19.1.1 gives it: its own session, `kind: "sub"`, its spawner's registry ID. */
function subCtx(pi: ReturnType<typeof fakePi>, id: string, parentId: string) {
	const base = ctxOf(pi, "sub");
	return {
		...base,
		agent: { kind: "sub", id, parentId },
		sessionManager: { ...base.sessionManager, getSessionId: () => `${id}-session`, getHeader: () => null },
		setTimeout: () => 0,
	};
}

test("a lane's subagent connects as sub with its parent session and its shell calls are judged by the hub", async () => {
	const lane = fakePi();
	bridge(lane, laneHub([], {}));
	await lane.emit("session_start", {}, laneCtx(lane, { provider: "fake", id: "primary" }, { n: 0 }));
	// OMP rebinds the extension for each subagent session: a second bridge in the same realm.
	const pi = fakePi();
	const sent: any[] = [];
	bridge(pi, laneHub(sent, {}));
	const ctx = subCtx(pi, "0-Worker", "a1");
	await pi.emit("session_start", {}, ctx);
	expect(sent[0].method).toBe("bridge.hello");
	expect(sent[0].params).toMatchObject({ agent: "sub", parent: "s1", session: "0-Worker-session" });
	expect(sent[0].params.token).toBeUndefined();
	expect(pi.handlers.get("tool_call")?.length).toBe(1);
	expect(await pi.emit("tool_call", { toolName: "bash", toolCallId: "w1", input: { command: "ls" } }, ctx)).toBeUndefined();
	const denied = await pi.emit("tool_call", { toolName: "bash", toolCallId: "w2", input: { command: "git push" } }, ctx);
	expect(denied).toEqual({ block: true, reason: "Blocked by shell rule 1 (command sha256 ab)" });
	expect(sent.filter(m => m.method === "bridge.shell").map(m => m.params.session)).toEqual(["0-Worker-session", "0-Worker-session"]);
});

test("a subagent of an accepted session whose hub gives no verdict runs no shell command", async () => {
	const lane = fakePi();
	bridge(lane, laneHub([], {}));
	await lane.emit("session_start", {}, laneCtx(lane, { provider: "fake", id: "primary" }, { n: 0 }));
	const pi = fakePi();
	bridge(pi, async () => Promise.reject(new Error("hub down")));
	const ctx = subCtx(pi, "1-Worker", "a1");
	await pi.emit("session_start", {}, ctx);
	const blocked = await pi.emit("tool_call", { toolName: "bash", toolCallId: "w3", input: { command: "ls" } }, ctx);
	expect(blocked?.block).toBe(true);
	// A subagent of a session the hub never accepted stays inert: no veto, no connection.
	const other = fakePi();
	const sent: any[] = [];
	bridge(other, laneHub(sent, {}));
	await other.emit("session_start", {}, subCtx(other, "2-Worker", "unknown-agent"));
	expect([...other.handlers.keys()].sort()).toEqual(["session_start", "session_switch"]);
	expect(sent.length).toBe(0);
});

test("the bridge connects only through the user's own private socket directory", async () => {
	const root = mkdtempSync(join(tmpdir(), "cataly-bridge-"));
	try {
		const uid = process.getuid?.() ?? -1;
		const dir = join(root, "run");
		mkdirSync(dir, { mode: 0o700 });
		const link = join(root, "link");
		symlinkSync(dir, link);
		expect(cataly.privateDir(dir, uid)).toBe(true);
		expect(cataly.privateDir(dir, uid + 1)).toBe(false);
		expect(cataly.privateDir(link, uid)).toBe(false);
		expect(cataly.privateDir(join(root, "missing"), uid)).toBe(false);
		const path = join(dir, "hub.sock");
		const server = Bun.listen({ unix: path, socket: { data() {} } });
		try {
			chmodSync(dir, 0o750);
			expect(cataly.privateDir(dir, uid)).toBe(false);
			await expect(cataly.unixConnect(path, () => {}, () => {})).rejects.toThrow("not private");
			chmodSync(dir, 0o700);
			await expect(cataly.unixConnect(join(link, "hub.sock"), () => {}, () => {})).rejects.toThrow("not private");
			const wire = await cataly.unixConnect(path, () => {}, () => {});
			wire.close();
		} finally {
			server.stop(true);
		}
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
});

/** A bridge as a hub launch starts it (launch token in the environment), on the given fake hub. */
function bridge(pi: ReturnType<typeof fakePi>, connect: cataly.Deps["connect"]): void {
	cataly.bridge(pi, { env: { CATALY_LAUNCH: "tok", CATALY_SOCKET: "/x.sock" }, connect, uid: () => 1 });
}
