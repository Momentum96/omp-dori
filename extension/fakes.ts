// Test fakes for the bridge (DESIGN 10.4): a fake OMP extension API, contexts and fake hub connections.
import type { Deps, Wire } from "./cataly";

type Handler = (event: any, ctx: any) => any;

export function fakePi() {
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

export function ctxOf(pi: ReturnType<typeof fakePi>, kind = "main") {
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

/** A fake hub for a lane: the pin, and verdicts as the hub gives them (it blocks `git push`, a
 * spawn from an unsaved session and any model but the pin or the announced fallback). */
export function laneHub(sent: any[], wires: { close?: () => void }): Deps["connect"] {
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

export function laneCtx(pi: ReturnType<typeof fakePi>, model: { provider: string; id: string }, aborts: { n: number }) {
	return { ...ctxOf(pi), models: { current: () => model }, abort: () => (aborts.n += 1) };
}
