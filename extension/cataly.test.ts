// Bridge tests (DESIGN 10.4): a fake OMP extension API and a fake hub connection. Run with
// `bun test extension/`.
import { expect, test } from "bun:test";
import { bridge, type Deps, fingerprint, socketPath, type Wire } from "./cataly";
import { ctxOf, fakePi, laneCtx, laneHub } from "./fakes";

/** A fake hub: answers hello with `role`, tools with an echo, stop with a reminder. */
function fakeHub(role: string, sent: any[], connects: { n: number }): Deps["connect"] {
	return async (_path, onLine, _onClose): Promise<Wire> => {
		connects.n += 1;
		return {
			write(line: string) {
				const msg = JSON.parse(line);
				sent.push(msg);
				const reply = (ok: unknown) => queueMicrotask(() => onLine(JSON.stringify({ v: 1, id: msg.id, ok })));
				if (msg.method === "bridge.hello")
					reply({ role, key: "k1", guidance: "GUIDE", tools: [{ name: "cataly_lead", description: "d", parameters: { type: "object" } }] });
				if (msg.method === "bridge.tool") reply({ text: `did ${msg.params.input.op}` });
				if (msg.method === "bridge.stop") reply({ context: msg.params.answered ? null : "owed" });
			},
			close() {},
		};
	};
}

// The 5 ms bound is asserted on the fastest of several runs: one run under host load can stall
// for longer while the code is fine (docs/build-notes.md, "Timing bounds under load").
test("an ordinary session registers only start and switch and never connects (check 11)", async () => {
	const costs: number[] = [];
	for (let run = 0; run < 7; run++) {
		const pi = fakePi();
		const connects = { n: 0 };
		const t0 = performance.now();
		bridge(pi, { env: {}, connect: fakeHub("lead", [], connects), uid: () => 1000 });
		await pi.emit("session_start", { type: "session_start" }, ctxOf(pi));
		costs.push(performance.now() - t0);
		expect([...pi.handlers.keys()].sort()).toEqual(["session_start", "session_switch"]);
		expect(connects.n).toBe(0);
		expect(pi.tools.length).toBe(0);
	}
	const cost = Math.min(...costs);
	expect(cost).toBeLessThan(5);
	console.log(`inert start-up cost: ${cost.toFixed(3)} ms (fastest of ${costs.map(c => c.toFixed(3)).join(", ")})`);
});

test("a launched session is accepted, gets its tool and guidance, forwards calls and the stop", async () => {
	const pi = fakePi();
	const sent: any[] = [];
	const connects = { n: 0 };
	const b = bridge(pi, { env: { CATALY_LAUNCH: "tok", CATALY_SOCKET: "/x.sock" }, connect: fakeHub("lead", sent, connects), uid: () => 1 });
	const ctx = ctxOf(pi);
	await pi.emit("session_start", {}, ctx);
	expect(connects.n).toBe(1);
	expect(sent[0].method).toBe("bridge.hello");
	expect(sent[0].params).toMatchObject({ ext: 1, token: "tok", session: "s1", agent: "main" });
	expect(b.token).toBeUndefined();
	expect(pi.tools.map(t => t.name)).toEqual(["cataly_lead"]);
	// A direct call only: never mounted as a bash command, and OMP runs it without asking.
	expect(pi.tools[0]).toMatchObject({ loadMode: "essential", approval: { tier: "write", policy: "allow" } });
	const prompt = await pi.emit("before_agent_start", { systemPrompt: ["BASE"] }, ctx);
	expect(prompt).toEqual({ systemPrompt: ["BASE", "GUIDE"] });
	expect(await pi.emit("before_agent_start", { systemPrompt: ["BASE", "GUIDE"] }, ctx)).toBeUndefined();
	expect(await pi.emit("before_agent_start", { systemPrompt: "BASE" }, ctx)).toBeUndefined();
	expect(await pi.emit("before_agent_start", { systemPrompt: ["BASE"] }, ctxOf(pi, "sub"))).toBeUndefined();
	const result = await pi.tools[0].execute("call-1", { op: "requests" }, undefined, undefined, ctx);
	expect(result.content[0].text).toBe("did requests");
	expect(sent.find(m => m.method === "bridge.tool").params).toMatchObject({ call: "call-1", tool: "cataly_lead", session: "s1" });
	expect(await pi.emit("session_stop", {}, ctx)).toEqual({ continue: true, additionalContext: "owed" });
	await pi.emit("message_end", { message: { role: "assistant", content: [{ type: "text", text: "done" }], stopReason: "stop" } }, ctx);
	expect(await pi.emit("session_stop", {}, ctx)).toBeUndefined();
	const end = sent.find(m => m.params?.kind === "message_end");
	expect(end.params).toMatchObject({ role: "assistant", text: "done", truncated: false });
});

test("role none leaves the session inert", async () => {
	const pi = fakePi();
	const connects = { n: 0 };
	bridge(pi, { env: { CATALY_LAUNCH: "used" }, connect: fakeHub("none", [], connects), uid: () => 1 });
	await pi.emit("session_start", {}, ctxOf(pi));
	expect(connects.n).toBe(1);
	expect([...pi.handlers.keys()].sort()).toEqual(["session_start", "session_switch"]);
	expect(pi.tools.length).toBe(0);
});

test("events are buffered while disconnected, oldest dropped first", () => {
	const pi = fakePi();
	const b = bridge(pi, { env: {}, connect: fakeHub("lead", [], { n: 0 }), uid: () => 1 });
	for (let i = 0; i < 1_005; i++) b.event("turn_end");
	expect(b.queue.length).toBe(1_000);
	expect(b.dropped).toBe(5);
});

test("fingerprints ignore timing and the socket path follows the hub's rule", () => {
	const a = fingerprint("bash", { content: [{ type: "text", text: "ok" }], details: { exit: 0, durationMs: 5 } });
	const b = fingerprint("bash", { content: [{ type: "text", text: "ok" }], details: { exit: 0, durationMs: 9 } });
	expect(a).toBe(b);
	expect(fingerprint("read", { content: [{ type: "text", text: "ok" }] })).not.toBe(a);
	expect(socketPath({ env: { XDG_RUNTIME_DIR: "/run/user/1" }, connect: fakeHub("x", [], { n: 0 }), uid: () => 7 })).toBe(
		"/run/user/1/cataly/hub.sock",
	);
	expect(socketPath({ env: {}, connect: fakeHub("x", [], { n: 0 }), uid: () => 7 })).toBe("/tmp/cataly-7/hub.sock");
});

test("a lane gets its tool, its shell veto fails closed, and the model barrier allows only the pin or the announced fallback", async () => {
	const pi = fakePi();
	const sent: any[] = [];
	const wires: { close?: () => void } = {};
	const aborts = { n: 0 };
	bridge(pi, { env: { CATALY_LAUNCH: "lane-token" }, connect: laneHub(sent, wires), uid: () => 1 });
	const model = { provider: "fake", id: "primary" };
	const ctx = laneCtx(pi, model, aborts);
	await pi.emit("session_start", {}, ctx);
	expect(pi.tools.map(t => t.name)).toEqual(["cataly_lane"]);
	expect(pi.handlers.has("tool_call")).toBe(true);
	// Shell rules: the hub decides; a denied command is blocked with the hub's reason.
	expect(await pi.emit("tool_call", { toolName: "bash", toolCallId: "c1", input: { command: "ls" } }, ctx)).toBeUndefined();
	const denied = await pi.emit("tool_call", { toolName: "bash", toolCallId: "c2", input: { command: "git push" } }, ctx);
	expect(denied).toEqual({ block: true, reason: "Blocked by shell rule 1 (command sha256 ab)" });
	expect(sent.filter(m => m.method === "bridge.shell").length).toBe(2);
	const malformed = await pi.emit("tool_call", { toolName: "bash", toolCallId: "c3", input: {} }, ctx);
	expect(malformed.block).toBe(true);
	// A subagent spawn is refused while the session is not on disk (EXT req. 34); the hub judges it.
	const spawn = await pi.emit("tool_call", { toolName: "task", toolCallId: "c4", input: {} }, ctx);
	expect(spawn.block).toBe(true);
	expect(sent.findLast(m => m.method === "bridge.shell").params).toMatchObject({ tool: "task", saved: false });
	// Other tools pay no round trip (EXT req. 35).
	const before = sent.length;
	expect(await pi.emit("tool_call", { toolName: "read", toolCallId: "c6", input: {} }, ctx)).toBeUndefined();
	expect(sent.length).toBe(before);
	// Model barrier: the hub judges the facts; a hold aborts the turn, the announced fallback passes.
	await pi.emit("turn_start", {}, ctx);
	expect(aborts.n).toBe(0);
	model.id = "other";
	await pi.emit("turn_start", {}, ctx);
	expect(aborts.n).toBe(1);
	expect(sent.findLast(m => m.method === "bridge.model").params).toMatchObject({ model: "fake/other", fallback: "" });
	await pi.emit("retry_fallback_applied", { to: "fake/other" }, ctx);
	await pi.emit("before_agent_start", { systemPrompt: ["BASE"] }, ctx);
	expect(aborts.n).toBe(1);
	// A bridge disconnect blocks every shell call (check 7).
	wires.close?.();
	const offline = await pi.emit("tool_call", { toolName: "bash", toolCallId: "c5", input: { command: "ls" } }, ctx);
	expect(offline.block).toBe(true);
	expect(offline.reason).toContain("No command ran");
});

/** A fake hub for a terminal session: accepts with `tern`, mirrors forms, and lets a test send commands; `first` comes in
 * the same read as the hello answer, as from a hub that queued it while answering. */
function ternHub(sent: any[], hub: { push?: (cmd: any) => void }, first?: Record<string, unknown>): Deps["connect"] {
	return async (_path, onLine): Promise<Wire> => {
		hub.push = cmd => onLine(JSON.stringify({ v: 1, id: 0, item: { cmd } }));
		return {
			write(line: string) {
				const msg = JSON.parse(line);
				sent.push(msg);
				const reply = (ok: unknown, after = () => {}) => queueMicrotask(() => (onLine(JSON.stringify({ v: 1, id: msg.id, ok })), after()));
				if (msg.method === "bridge.hello") reply({ role: "lead", key: "k3", tools: [], tern: true }, () => first && hub.push?.(first));
				if (msg.method === "bridge.form") reply({ mirrored: true });
			},
			close() {},
		};
	};
}

function ternCtx(pi: ReturnType<typeof fakePi>, state: { idle: boolean; aborts: number; shutdowns: number; goal: string; objective?: string }) {
	return {
		...ctxOf(pi),
		mode: "tui",
		isIdle: () => state.idle,
		abort: () => (state.aborts += 1),
		shutdown: () => (state.shutdowns += 1),
		// `ctx.models.resolve` accepts a thinking suffix and returns the base model.
		models: { current: () => ({ provider: "fake", id: "primary" }), resolve: (spec: string) => (spec.startsWith("fake/") ? { provider: "fake", id: spec.slice(5).split(":")[0] } : undefined) },
		// As stock OMP 19.1.1: a mode change is refused while a turn runs.
		sessionModes: {
			getState: () => ({ goal: { status: state.goal, objective: state.objective } }),
			set: async (r: { action: string; objective?: string }) => {
				if (!state.idle) return { ok: false, code: "busy", message: "The session is busy with a turn" };
				state.goal = r.action === "pause" ? "paused" : "active";
				if (r.action === "enter") state.objective = r.objective;
				return { ok: true, changed: true };
			},
		},
	};
}

const settle = () => new Promise(r => setTimeout(r, 5));
const replies = (sent: any[]) => sent.filter(m => m.method === "bridge.reply").map(m => m.params);

test("a terminal session takes deliveries, steer, interrupt, model, goal and shutdown from the hub (check 2)", async () => {
	const pi: any = fakePi();
	const calls: any[] = [];
	pi.sendMessage = async (m: any, o: any) => void calls.push(["message", m, o]);
	pi.sendUserMessage = async (t: string, o: any) => void calls.push(["user", t, o]);
	pi.setModel = async (m: any) => void calls.push(["model", m]);
	const sent: any[] = [];
	const hub: { push?: (cmd: any) => void } = {};
	const state = { idle: true, aborts: 0, shutdowns: 0, goal: "active" };
	const b = bridge(pi, { env: { CATALY_LAUNCH: "t" }, connect: ternHub(sent, hub), uid: () => 1 });
	await pi.emit("session_start", {}, ternCtx(pi, state));
	expect(b.tern).toBe(true);
	expect(sent[0].params).toMatchObject({ idle: true, model: "fake/primary", goal: { status: "active" } });
	const push = (cmd: any) => hub.push?.({ session: "s1", ...cmd });
	push({ n: 1, op: "deliver", id: "d1", text: "[cataly:d1] hi", mode: "wake" });
	push({ n: 2, op: "deliver", id: "d1", text: "[cataly:d1] hi", mode: "wake" });
	push({ n: 3, op: "deliver", id: "d1", text: "other", mode: "wake" });
	push({ n: 4, op: "deliver", id: "d2", text: "steer me", mode: "steer" });
	push({ n: 5, op: "interrupt" });
	push({ n: 6, op: "model", model: "fake/second:high" });
	push({ n: 7, op: "goal", action: "pause" });
	push({ n: 8, op: "deliver", id: "d3", text: "x", mode: "wake", session: "elsewhere" });
	await settle();
	state.idle = false;
	push({ n: 9, op: "deliver", id: "d4", text: "steer now", mode: "steer" });
	push({ n: 10, op: "shutdown" });
	push({ n: 11, op: "interrupt" });
	push({ n: 12, op: "shutdown", force: true });
	push({ n: 13, op: "goal", action: "enter", objective: "Do it" });
	await settle();
	state.idle = true;
	push({ n: 14, op: "goal", action: "enter", objective: "Do it" });
	await settle();
	const r = replies(sent);
	expect(r.map(x => x.n)).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
	expect(r[12].refused).toBe("busy: The session is busy with a turn");
	expect(r[13]).toMatchObject({ goal: { status: "active", objective: "Do it" } });
	expect(r[0]).toEqual({ n: 1, sent: true });
	expect(r[1]).toEqual({ n: 2, sent: true });
	expect(r[2].refused).toContain("other text");
	expect(r[3].refused).toBe("no running turn to steer");
	expect(r[4]).toMatchObject({ idle: true });
	expect(r[5]).toEqual({ n: 6, model: "fake/primary" });
	expect(r[6]).toMatchObject({ goal: { status: "paused" } });
	expect(r[7]).toEqual({ n: 8, refused: "not current" });
	expect(r[8]).toEqual({ n: 9, sent: true });
	expect(r[9].refused).toContain("busy");
	expect(state.aborts).toBe(1);
	expect(state.shutdowns).toBe(1);
	expect(calls.filter(c => c[0] === "message").length).toBe(1);
	expect(calls[0][1]).toMatchObject({ customType: "cataly-delivery", content: "[cataly:d1] hi", details: { delivery: "d1" } });
	expect(calls[0][2]).toEqual({ triggerTurn: true, deliverAs: "followUp" });
	expect(calls.find(c => c[0] === "user")).toEqual(["user", "steer now", { deliverAs: "steer" }]);
	expect(calls.find(c => c[0] === "model")[1]).toEqual({ provider: "fake", id: "second" });
	// The delivered custom message reports its delivery ID and file as the receipt (EXT req. 27).
	const ctx = ternCtx(pi, state);
	await pi.emit("message_end", { message: { role: "custom", display: true, content: "[cataly:d1] hi", details: { delivery: "d1" } } }, ctx);
	expect(sent.findLast(m => m.params?.kind === "message_end").params).toMatchObject({ delivery: "d1", file: "/s/s1.jsonl" });
});

test("a command in the same read as the hello answer runs once the answer is applied", async () => {
	const pi = fakePi();
	const sent: unknown[] = [];
	const hub: { push?: (cmd: unknown) => void } = {};
	const state = { idle: true, aborts: 0, shutdowns: 0, goal: "inactive" };
	const first = { n: 1, session: "s1", op: "goal", action: "enter", objective: "Do it" };
	bridge(pi, { env: { CATALY_LAUNCH: "t" }, connect: ternHub(sent, hub, first), uid: () => 1 });
	await pi.emit("session_start", {}, ternCtx(pi, state));
	await settle();
	expect(replies(sent)).toMatchObject([{ n: 1, goal: { status: "active", objective: "Do it" } }]);
});

test("a user session switch is fenced: refused while a command runs, deliveries refused while it changes (check 2)", async () => {
	const pi: any = fakePi();
	const gate = Promise.withResolvers<void>();
	pi.sendMessage = async () => gate.promise;
	const sent: any[] = [];
	const hub: { push?: (cmd: any) => void } = {};
	const state = { idle: true, aborts: 0, shutdowns: 0, goal: "inactive" };
	const b = bridge(pi, { env: { CATALY_LAUNCH: "t" }, connect: ternHub(sent, hub), uid: () => 1 });
	const ctx = ternCtx(pi, state);
	await pi.emit("session_start", {}, ctx);
	hub.push?.({ n: 1, session: "s1", op: "deliver", id: "d1", text: "slow", mode: "wake" });
	await settle();
	expect(await pi.emit("session_before_switch", {}, ctx)).toEqual({ cancel: true });
	gate.resolve();
	await settle();
	expect(await pi.emit("session_before_switch", {}, ctx)).toBeUndefined();
	expect(b.changing).not.toBe(0);
	hub.push?.({ n: 2, session: "s1", op: "deliver", id: "d2", text: "now", mode: "wake" });
	await settle();
	expect(replies(sent).find(r => r.n === 2)).toEqual({ n: 2, refused: "not current" });
	await pi.emit("session_switch", {}, ctx);
	expect(b.changing).toBe(0);
	await pi.emit("session_clear", {}, ctx);
	expect(sent.some(m => m.params?.kind === "session_clear")).toBe(true);
});

test("a mirrored form takes the first answer and withdraws the other (check 4)", async () => {
	const pi: any = fakePi();
	const sent: any[] = [];
	const hub: { push?: (cmd: any) => void } = {};
	const state = { idle: false, aborts: 0, shutdowns: 0, goal: "inactive" };
	bridge(pi, { env: { CATALY_LAUNCH: "t" }, connect: ternHub(sent, hub), uid: () => 1 });
	let [aborted, shown]: [boolean, unknown] = [false, undefined];
	const ctx: any = ternCtx(pi, state);
	ctx.ui = {
		...ctx.ui,
		askDialog: (questions: unknown, o: { signal: AbortSignal }) => {
			shown = questions;
			const local = Promise.withResolvers<unknown>();
			o.signal.addEventListener("abort", () => ((aborted = true), local.resolve(undefined)));
			return local.promise;
		},
	};
	await pi.emit("session_start", {}, ctx);
	const input = { questions: [{ id: "q", question: "Color?", options: [{ label: "Red" }] }] };
	const pending = pi.emit("tool_call", { toolName: "ask", toolCallId: "f1", input }, ctx);
	await settle();
	expect(sent.find(m => m.method === "bridge.form").params).toMatchObject({ call: "f1", input });
	hub.push?.({ n: 1, op: "form", call: "f1", answer: { value: "Red" } });
	const result = await pending;
	expect(result.block).toBe(true);
	expect(result.reason).toContain('"Red"');
	expect(aborted).toBe(true);
	// Stock OMP's `ui.askDialog` takes the question list itself.
	expect(shown).toEqual(input.questions);
	expect(sent.find(m => m.params?.kind === "form_closed").params).toMatchObject({ call: "f1", by: "Discord" });
	hub.push?.({ n: 2, op: "form", call: "f1", answer: {} });
	expect(replies(sent).find(r => r.n === 2).refused).toContain("already answered");
});
