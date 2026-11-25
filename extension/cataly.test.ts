// Bridge tests (DESIGN 10.4): a fake OMP extension API and a fake hub connection. Run with
// `bun test extension/`.
import { expect, test } from "bun:test";
import { bridge, type Deps, fingerprint, socketPath, type Wire } from "./cataly";

type Handler = (event: any, ctx: any) => any;

function fakePi() {
	const handlers = new Map<string, Handler[]>();
	const tools: any[] = [];
	const entries: any[] = [];
	let active = ["read", "bash"];
	return {
		handlers,
		tools,
		entries,
		active: () => active,
		on(name: string, h: Handler) {
			handlers.set(name, [...(handlers.get(name) ?? []), h]);
		},
		registerTool(def: any) {
			tools.push(def);
		},
		getActiveTools: () => active,
		setActiveTools(names: string[]) {
			active = names;
		},
		appendEntry(customType: string, data: unknown) {
			entries.push({ type: "custom", customType, data });
		},
		async emit(name: string, event: any, ctx: any) {
			let out: any;
			for (const h of handlers.get(name) ?? []) out = (await h(event, ctx)) ?? out;
			return out;
		},
	};
}

function ctxOf(pi: ReturnType<typeof fakePi>, kind = "main") {
	return {
		agent: { kind, id: "a1" },
		cwd: "/work",
		mode: "rpc",
		sessionManager: { getSessionId: () => "s1", getSessionFile: () => "/s/s1.jsonl", getBranch: () => pi.entries },
		ui: { notify() {}, setStatus() {} },
		isIdle: () => true,
		setInterval: () => 0,
		setTimeout: (fn: () => void, ms: number) => setTimeout(fn, ms),
	};
}

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
					reply({ role, key: "k1", guidance: "GUIDE", tools: [{ name: "cataly", description: "d", parameters: { type: "object" } }] });
				if (msg.method === "bridge.tool") reply({ text: `did ${msg.params.input.op}` });
				if (msg.method === "bridge.stop") reply({ context: msg.params.answered ? null : "owed" });
			},
			close() {},
		};
	};
}

test("an ordinary session registers only start and switch and never connects (check 11)", async () => {
	const pi = fakePi();
	const connects = { n: 0 };
	const t0 = performance.now();
	bridge(pi, { env: {}, connect: fakeHub("lead", [], connects), uid: () => 1000 });
	await pi.emit("session_start", { type: "session_start" }, ctxOf(pi));
	const cost = performance.now() - t0;
	expect([...pi.handlers.keys()].sort()).toEqual(["session_start", "session_switch"]);
	expect(connects.n).toBe(0);
	expect(pi.tools.length).toBe(0);
	expect(cost).toBeLessThan(5);
	console.log(`inert start-up cost: ${cost.toFixed(3)} ms`);
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
	expect(pi.tools.map(t => t.name)).toEqual(["cataly"]);
	const prompt = await pi.emit("before_agent_start", { systemPrompt: ["BASE"] }, ctx);
	expect(prompt).toEqual({ systemPrompt: ["BASE", "GUIDE"] });
	expect(await pi.emit("before_agent_start", { systemPrompt: ["BASE", "GUIDE"] }, ctx)).toBeUndefined();
	expect(await pi.emit("before_agent_start", { systemPrompt: "BASE" }, ctx)).toBeUndefined();
	expect(await pi.emit("before_agent_start", { systemPrompt: ["BASE"] }, ctxOf(pi, "sub"))).toBeUndefined();
	const result = await pi.tools[0].execute("call-1", { op: "requests" }, undefined, undefined, ctx);
	expect(result.content[0].text).toBe("did requests");
	expect(sent.find(m => m.method === "bridge.tool").params).toMatchObject({ call: "call-1", tool: "cataly", session: "s1" });
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

/** A fake hub for a lane: the pin, and verdicts as the hub gives them (it blocks `git push`, a
 * spawn from an unsaved session and any model but the pin or the announced fallback). */
function laneHub(sent: any[], wires: { close?: () => void }): Deps["connect"] {
	return async (_path, onLine, onClose): Promise<Wire> => {
		wires.close = onClose;
		return {
			write(line: string) {
				const msg = JSON.parse(line);
				sent.push(msg);
				const reply = (ok: unknown) => queueMicrotask(() => onLine(JSON.stringify({ v: 1, id: msg.id, ok })));
				const p = msg.params;
				if (msg.method === "bridge.hello")
					reply({
						role: "lane",
						key: "k2",
						guidance: "LANE",
						tools: [{ name: "cataly_lane", parameters: { type: "object" }, timeout_ms: 10_000 }],
						pin: "fake/primary",
						shell: true,
					});
				if (msg.method === "bridge.shell") {
					const allow = p.tool === "task" ? p.saved : typeof p.command === "string" && !p.command.includes("git push");
					reply({ allow, reason: allow ? "" : "Blocked by shell rule 1 (command sha256 ab)" });
				}
				if (msg.method === "bridge.model") reply({ allow: p.model === "fake/primary" || p.model === p.fallback });
			},
			close() {},
		};
	};
}

function laneCtx(pi: ReturnType<typeof fakePi>, model: { provider: string; id: string }, aborts: { n: number }) {
	return { ...ctxOf(pi), models: { current: () => model }, abort: () => (aborts.n += 1) };
}

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
