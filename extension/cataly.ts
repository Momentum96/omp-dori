// Cataly bridge for OMP (DESIGN 3.5, 10.6; EXT-01 to EXT-37 kept parts). Loaded by every OMP
// process from the global extension folder. It stays inert unless the session carries a Cataly
// launch token or the Cataly session marker, and the hub accepts it: until then it has only the
// session start and switch handlers, no timer, no connection and no prompt text. An accepted
// session forwards events, offers the hub's tools, appends the hub's guidance to the system
// prompt, asks the hub at stop, and keeps tool-result fingerprints. Every decision is the hub's.

const PROTOCOL = 1;
const MARKER = "cataly";
const CONNECT_MS = 1_000;
const HEARTBEAT_MS = 15_000;
const SILENT_MS = 45_000;
const BUFFER_EVENTS = 1_000;
const BUFFER_BYTES = 1 << 20;
const TEXT_CUT = 2_048;
const BACKOFF_S = [1, 2, 4, 8, 16, 30];
const TOOL_MS = 5_000;
const SHELL_MS = 2_000;
const STOP_MS = 1_000;
const SHUTDOWN_MS = 500;
const CALLS_KEPT = 4_096;
const LINE_MAX = 1 << 20;
const UNSURE = "nothing was recorded or started unless the hub reports otherwise. Retrying is safe.";

type Json = Record<string, unknown>;
type Ctx = any;
type Pi = any;

/** One stream connection, as returned by `Deps.connect`. */
export interface Wire {
	write(line: string): void;
	close(): void;
}

export interface Deps {
	env: Record<string, string | undefined>;
	connect(path: string, onLine: (line: string) => void, onClose: () => void): Promise<Wire>;
	uid(): number;
}

/** Bun's Unix socket client, line-framed. */
async function unixConnect(
	path: string,
	onLine: (line: string) => void,
	onClose: () => void,
): Promise<Wire> {
	let buf = "";
	let closed = false;
	const end = () => {
		if (!closed) {
			closed = true;
			onClose();
		}
	};
	const sock = await withTimeout(
		Bun.connect({
			unix: path,
			socket: {
				data(_s, chunk) {
					buf += chunk.toString();
					if (buf.length > LINE_MAX * 2) {
						buf = "";
						_s.end();
						return;
					}
					let at = buf.indexOf("\n");
					while (at >= 0) {
						const line = buf.slice(0, at);
						buf = buf.slice(at + 1);
						if (line.trim()) onLine(line);
						at = buf.indexOf("\n");
					}
				},
				close: end,
				error: end,
			},
		}),
		CONNECT_MS,
	);
	if (!sock) throw new Error("the hub did not accept a connection within 1 s");
	return {
		write: line => {
			if (!closed) sock.write(line);
		},
		close: () => {
			sock.end();
			end();
		},
	};
}

function withTimeout<T>(p: Promise<T>, ms: number): Promise<T | undefined> {
	const { promise, resolve } = Promise.withResolvers<T | undefined>();
	const timer = setTimeout(() => resolve(undefined), ms);
	p.then(resolve, () => resolve(undefined)).finally(() => clearTimeout(timer));
	return promise;
}

/** The hub socket: the launch's `CATALY_SOCKET`, else the per-user default (EXT-33). */
export function socketPath(deps: Deps): string {
	const env = deps.env;
	if (env.CATALY_SOCKET) return env.CATALY_SOCKET;
	const run = env.XDG_RUNTIME_DIR;
	const candidate = run ? `${run}/cataly/hub.sock` : "";
	if (candidate && candidate.length < (process.platform === "darwin" ? 104 : 108)) return candidate;
	return `/tmp/cataly-${deps.uid()}/hub.sock`;
}

/** Runs `fn`; a throw from OMP's side never escapes into the session. */
function safe<T>(fn: () => T): T | undefined {
	try {
		return fn();
	} catch {
		return undefined;
	}
}

function marked(ctx: Ctx): boolean {
	const branch: Json[] = safe(() => ctx.sessionManager?.getBranch?.()) ?? [];
	return branch.some(e => e?.type === "custom" && e?.customType === MARKER);
}

function textOf(content: unknown): string {
	if (typeof content === "string") return content;
	if (!Array.isArray(content)) return "";
	return content
		.filter((b: Json) => b?.type === "text" && typeof b.text === "string")
		.map((b: Json) => b.text as string)
		.join("\n");
}

function cut(text: string): { text: string; truncated: boolean } {
	return text.length > TEXT_CUT ? { text: text.slice(0, TEXT_CUT), truncated: true } : { text, truncated: false };
}

/** Drops run timing, job bookkeeping and file locations from result details (EVR req. 13). */
function stable(value: unknown): unknown {
	if (Array.isArray(value)) return value.map(stable);
	if (!value || typeof value !== "object") return value;
	const out: Json = {};
	for (const key of Object.keys(value as Json).sort()) {
		if (/^(duration|elapsed|startedAt|endedAt|timestamp|time|jobId|outputPath|artifactPath|path)/i.test(key))
			continue;
		out[key] = stable((value as Json)[key]);
	}
	return out;
}

/** SHA-256 over the tool name, visible content and structured details (EVR req. 13). */
export function fingerprint(tool: string, result: Json | undefined): string {
	const hasher = new Bun.CryptoHasher("sha256");
	hasher.update(JSON.stringify([tool, stable(result?.content ?? null), stable(result?.details ?? null)]));
	return hasher.digest("hex");
}

interface Call {
	tool: string;
	cwd: string;
	starts: number;
	command?: string;
	fingerprint?: string;
	error?: boolean;
}

/** The model OMP runs for this context, `provider/id`, or "" when unknown. */
function modelOf(ctx: Ctx): string {
	const m = safe(() => ctx?.models?.current?.() ?? ctx?.model);
	return m ? `${m.provider}/${m.id}` : "";
}

/** The bridge state of one OMP process. */
export class Bridge {
	readonly pi: Pi;
	readonly deps: Deps;
	wired = false;
	role = "none";
	key: string | undefined;
	token: string | undefined;
	guidance = "";
	session = "";
	ctx: Ctx;
	wire: Wire | undefined;
	connecting = false;
	shutting = false;
	next = 1;
	seq = 0;
	pending = new Map<number, (v: Json | undefined) => void>();
	queue: string[] = [];
	queued = 0;
	dropped = 0;
	lastRx = 0;
	attempt = 0;
	warned = false;
	calls = new Map<string, Call>();
	lastAnswer = false;
	pin: string | undefined;
	shell = false;
	fallbackTo = "";

	constructor(pi: Pi, deps: Deps) {
		this.pi = pi;
		this.deps = deps;
		this.token = deps.env.CATALY_LAUNCH || undefined;
	}

	/** Session start or switch: a candidate connects; anything else stays inert (EXT req. 1-3). */
	async consider(ctx: Ctx, transition?: Json): Promise<void> {
		if (ctx?.agent?.kind === "sub") return;
		const id = String(ctx.sessionManager?.getSessionId?.() ?? "");
		if (this.wired) {
			this.ctx = ctx;
			if (id !== this.session) {
				this.calls.clear();
				this.session = id;
				const answer = await this.request("bridge.switch", { session: id, file: this.file(), ...transition }, TOOL_MS);
				this.apply(answer);
			}
			return;
		}
		if (!this.token && !marked(ctx)) return;
		this.ctx = ctx;
		this.session = id;
		await this.connect();
	}

	file(): string {
		return String(this.ctx?.sessionManager?.getSessionFile?.() ?? "");
	}

	hello(): Json {
		const ctx = this.ctx;
		const [cwd, agent, mode] = [ctx?.cwd ?? "", ctx?.agent?.kind ?? "main", ctx?.mode ?? ""].map(String);
		const ids = { token: this.token, key: this.key, session: this.session, file: this.file() };
		return { ext: PROTOCOL, ...ids, pid: process.pid, cwd, agent, mode, marker: marked(ctx) };
	}

	async connect(): Promise<void> {
		if (this.connecting || this.shutting) return;
		this.connecting = true;
		try {
			this.wire = await this.deps.connect(socketPath(this.deps), line => this.receive(line), () => this.closed());
		} catch {
			this.wire = undefined;
		}
		this.connecting = false;
		if (!this.wire) return this.retry();
		this.lastRx = Date.now();
		const answer = await this.request("bridge.hello", this.hello(), TOOL_MS, true);
		this.token = undefined;
		if (!answer) {
			this.wire?.close();
			return;
		}
		if (answer.role !== "lead" && answer.role !== "lane") {
			// Role none: inert for this session (EXT req. 4).
			this.role = "none";
			this.key = undefined;
			this.wire?.close();
			this.wire = undefined;
			return;
		}
		this.attempt = 0;
		this.key = String(answer.key ?? "");
		this.apply(answer);
		if (!this.wired) this.wire_up(answer);
		else this.resend();
		if (this.warned) {
			this.warned = false;
			this.status(answer.status as string | undefined);
		}
	}

	apply(answer: Json | undefined): void {
		if (!answer) return;
		this.role = String(answer.role ?? this.role);
		if (typeof answer.guidance === "string") this.guidance = answer.guidance;
		if (typeof answer.pin === "string") this.pin = answer.pin;
		if (answer.shell !== undefined) this.shell = Boolean(answer.shell);
		if (this.role !== "none") this.mark();
	}

	/** Reconnect: snapshot first, then what was buffered, then how much was lost (EXT req. 13-14). */
	resend(): void {
		this.send("bridge.event", {
			kind: "snapshot",
			session: this.session,
			role: this.role,
			idle: Boolean(this.ctx?.isIdle?.()),
			model: modelOf(this.ctx) || undefined,
			last_seq: this.seq,
		});
		const queued = this.queue;
		this.queue = [];
		this.queued = 0;
		for (const line of queued) this.wire?.write(line);
		if (this.dropped > 0) {
			this.event("dropped", { count: this.dropped });
			this.dropped = 0;
		}
	}

	retry(): void {
		if (this.shutting) return;
		if (!this.warned) {
			this.warned = true;
			safe(() => this.ctx?.ui?.notify?.("Cataly hub not reachable; retrying.", "warning"));
			this.status("Cataly offline");
		}
		const wait = BACKOFF_S[Math.min(this.attempt, BACKOFF_S.length - 1)] * 1000;
		this.attempt += 1;
		this.timer(() => void this.connect(), wait, false);
	}

	closed(): void {
		this.wire = undefined;
		for (const resolve of this.pending.values()) resolve(undefined);
		this.pending.clear();
		if (this.wired && !this.shutting) this.retry();
	}

	receive(line: string): void {
		this.lastRx = Date.now();
		let msg: Json;
		try {
			msg = JSON.parse(line);
		} catch {
			return;
		}
		const id = Number(msg.id ?? 0);
		const resolve = this.pending.get(id);
		if (resolve) return resolve(msg.err ? undefined : ((msg.ok as Json) ?? {}));
		const item = msg.item as Json | undefined;
		if (item && typeof item.status === "string") this.status(item.status);
	}

	line(method: string, params: Json, id = 0): string {
		return `${JSON.stringify({ v: 1, id, method, params })}\n`;
	}

	send(method: string, params: Json): void {
		this.wire?.write(this.line(method, params));
	}

	/** One event, numbered; buffered while disconnected, oldest dropped first (EXT req. 13). */
	event(kind: string, fields: Json = {}): void {
		this.seq += 1;
		const line = this.line("bridge.event", { seq: this.seq, session: this.session, kind, ...fields });
		if (this.wire) return this.wire.write(line);
		this.queue.push(line);
		this.queued += line.length;
		while (this.queue.length > BUFFER_EVENTS || this.queued > BUFFER_BYTES) {
			this.queued -= this.queue.shift()?.length ?? 0;
			this.dropped += 1;
		}
	}

	/** A request answered by the hub, or `undefined` on timeout or disconnect. */
	request(method: string, params: Json, ms: number, early = false): Promise<Json | undefined> {
		if (!this.wire || (!early && !this.key)) return Promise.resolve(undefined);
		const id = this.next++;
		const { promise, resolve } = Promise.withResolvers<Json | undefined>();
		const timer = setTimeout(() => this.pending.get(id)?.(undefined), ms);
		this.pending.set(id, v => {
			this.pending.delete(id);
			clearTimeout(timer);
			resolve(v);
		});
		this.wire.write(this.line(method, params, id));
		return promise;
	}

	timer(fn: () => void, ms: number, repeat: boolean): void {
		const guarded = () => void safe(fn);
		const ctx = this.ctx;
		if (repeat) ctx?.setInterval?.(guarded, ms) ?? setInterval(guarded, ms);
		else ctx?.setTimeout?.(guarded, ms) ?? setTimeout(guarded, ms);
	}

	status(text: string | undefined): void {
		safe(() => this.ctx?.ui?.setStatus?.("cataly", text || undefined));
	}

	/** The session marker, once per branch, so a resume by hand is recognised (EXT req. 6). */
	mark(): void {
		if (this.ctx && !marked(this.ctx)) safe(() => this.pi.appendEntry(MARKER, { v: PROTOCOL }));
	}

	/** After the first acceptance: handlers, tools and the heartbeat (EXT req. 4, 15, 17). */
	wire_up(answer: Json): void {
		this.wired = true;
		const pi = this.pi;
		const tools = Array.isArray(answer.tools) ? (answer.tools as Json[]) : [];
		const names: string[] = [];
		for (const def of tools) {
			const name = String(def.name ?? "");
			if (!name) continue;
			names.push(name);
			// The hub sets each tool's answer deadline (a lane report reads evidence files).
			const ms = Number(def.timeout_ms) || TOOL_MS;
			pi.registerTool({
				name,
				label: String(def.label ?? name),
				description: String(def.description ?? ""),
				parameters: def.parameters ?? { type: "object" },
				execute: (callId: string, params: Json, _signal: unknown, _update: unknown, ctx: Ctx) =>
					this.tool(name, ms, callId, params, ctx),
			});
		}
		try {
			const active: string[] = pi.getActiveTools?.() ?? [];
			pi.setActiveTools?.([...active.filter((n: string) => !names.includes(n)), ...names]);
		} catch {}
		this.status(answer.status as string | undefined);
		this.timer(() => this.beat(), HEARTBEAT_MS, true);
		pi.on("before_agent_start", async (e: Json, ctx: Ctx) => {
			await this.barrier(ctx);
			return this.guide(e, ctx);
		});
		// Lane hooks, added only once the hub said they apply (EXT req. 1, 32, 34, 38).
		if (this.shell) pi.on("tool_call", (e: Json, ctx: Ctx) => this.veto(e, ctx));
		if (this.pin) pi.on("retry_fallback_applied", (e: Json) => void (this.fallbackTo = String(e.to ?? "")));
		pi.on("session_stop", (_e: Json, ctx: Ctx) => this.stop(ctx));
		pi.on("session_shutdown", (_e: Json, ctx: Ctx) => this.shutdown(ctx));
		pi.on("session_compact", (_e: Json, ctx: Ctx) => this.main(ctx) && this.event("session_compact"));
		pi.on("session_branch", (e: Json, ctx: Ctx) => this.transition("session_branch", e, ctx));
		pi.on("session_tree", (e: Json, ctx: Ctx) => this.transition("session_tree", e, ctx));
		pi.on("goal_updated", (e: Json, ctx: Ctx) => {
			const goal = (e.goal ?? e) as Json;
			this.main(ctx) && this.event("goal_updated", { status: goal.status, objective: goal.objective });
		});
		pi.on("agent_start", (_e: Json, ctx: Ctx) => this.main(ctx) && this.event("agent_start"));
		pi.on("agent_end", (e: Json, ctx: Ctx) =>
			this.main(ctx) &&
			this.event("agent_end", { will_continue: e.willContinue ?? e.yielded, awaiting: e.awaitingAsyncWork }),
		);
		pi.on("turn_start", async (_e: Json, ctx: Ctx) => {
			await this.barrier(ctx);
			this.main(ctx) && this.event("turn_start");
		});
		pi.on("turn_end", (_e: Json, ctx: Ctx) => this.main(ctx) && this.event("turn_end"));
		pi.on("message_end", (e: Json, ctx: Ctx) => this.message(e, ctx));
		pi.on("tool_execution_start", (e: Json, ctx: Ctx) => this.toolStart(e, ctx));
		pi.on("tool_execution_end", (e: Json, ctx: Ctx) => this.toolEnd(e, ctx));
		const observed = ["tool_approval_requested", "tool_approval_resolved", "plan_review_requested", "plan_review_resolved"];
		for (const kind of observed) {
			const fields = (e: Json) => ({ call: e.toolCallId ?? e.reviewId, tool: e.toolName, choice: e.choice });
			pi.on(kind, (e: Json, ctx: Ctx) => this.main(ctx) && this.event(kind, fields(e)));
		}
	}

	main(ctx: Ctx): boolean {
		return this.role !== "none" && ctx?.agent?.kind !== "sub";
	}

	beat(): void {
		if (!this.wire) return;
		if (Date.now() - this.lastRx > SILENT_MS) {
			this.wire.close();
			return;
		}
		this.send("bridge.ping", {});
	}

	transition(kind: string, e: Json, ctx: Ctx): void {
		if (!this.main(ctx)) return;
		this.calls.clear();
		this.session = String(ctx.sessionManager?.getSessionId?.() ?? this.session);
		this.event(kind, { file: this.file(), reason: e.reason });
	}

	/** Shell and spawn calls (EXT req. 32-35): the hub judges their facts; no answer blocks. */
	async veto(e: Json, ctx: Ctx): Promise<Json | undefined> {
		const tool = String(e.toolName ?? "");
		if (tool !== "bash" && tool !== "task") return undefined;
		const file = this.file();
		const saved = tool === "task" && file !== "" && (await Bun.file(file).exists());
		const command = (e.input as Json | undefined)?.command;
		const params = { session: this.session, agent: ctx?.agent?.id, call: e.toolCallId, tool, command, saved };
		const answer = await this.request("bridge.shell", params, SHELL_MS);
		if (answer?.allow) return undefined;
		return { block: true, reason: String(answer?.reason || "No command ran: Cataly did not answer within 2 s. Report a blocker (kind waiting).") };
	}

	/** Model barrier (EXT req. 38-39): the hub judges what OMP runs; a hold or no answer aborts. */
	async barrier(ctx: Ctx): Promise<void> {
		if (!this.pin || ctx?.agent?.kind === "sub") return;
		const thinking = safe(() => this.pi.getThinkingLevel?.());
		const facts = { session: this.session, model: modelOf(ctx), thinking, fallback: this.fallbackTo };
		const answer = await this.request("bridge.model", facts, SHELL_MS);
		if (!answer?.allow) safe(() => ctx?.abort?.());
	}

	/** Appends the hub's guidance once to the prompt list OMP passes in (EXT req. 36). */
	guide(e: Json, ctx: Ctx): Json | undefined {
		const prompt = e.systemPrompt;
		if (!this.main(ctx) || !this.guidance || !Array.isArray(prompt) || prompt.includes(this.guidance)) return;
		return prompt.every(b => typeof b === "string") ? { systemPrompt: [...prompt, this.guidance] } : undefined;
	}

	message(e: Json, ctx: Ctx): void {
		if (!this.main(ctx)) return;
		const m = (e.message ?? e) as Json;
		const role = String(m.role ?? "");
		if (role !== "user" && role !== "assistant" && !(role === "custom" && m.display)) return;
		const visible = cut(textOf(m.content));
		const stop = m.stopReason as string | undefined;
		if (role === "assistant") this.lastAnswer = visible.text.trim() !== "" && stop === "stop";
		const entry = e.entryId ?? m.id;
		this.event("message_end", { role, ...visible, stop, entry, at: m.timestamp });
	}

	toolStart(e: Json, ctx: Ctx): void {
		if (!this.main(ctx)) return;
		const id = String(e.toolCallId ?? "");
		const tool = String(e.toolName ?? "");
		const seen = this.calls.get(id);
		if (seen) seen.starts += 1;
		else {
			const command = tool === "bash" ? String((e.args as Json | undefined)?.command ?? "").slice(0, TEXT_CUT) : undefined;
			this.calls.set(id, { tool, cwd: String(ctx.cwd ?? ""), starts: 1, command });
		}
		while (this.calls.size > CALLS_KEPT) {
			const oldest = this.calls.keys().next().value;
			if (oldest === undefined) break;
			this.calls.delete(oldest);
		}
		this.event("tool_execution_start", { call: id, tool, cwd: ctx.cwd });
	}

	toolEnd(e: Json, ctx: Ctx): void {
		if (!this.main(ctx)) return;
		const id = String(e.toolCallId ?? "");
		const call = this.calls.get(id);
		const result = e.result as Json | undefined;
		if (call) {
			call.error = Boolean(e.isError);
			call.fingerprint = fingerprint(call.tool, result);
		}
		const job = (result?.details as Json | undefined)?.jobId;
		this.event("tool_execution_end", { call: id, tool: e.toolName, error: Boolean(e.isError), job });
	}

	/** Facts for the calls a tool names in `calls` (EXT req. 21-22); the hub judges them. */
	evidence(params: Json): Json[] | undefined {
		const ids = params?.calls;
		if (!Array.isArray(ids)) return undefined;
		return ids.slice(0, 64).map(raw => {
			const call = this.calls.get(String(raw));
			return call ? { call: String(raw), seen: true, ...call, ambiguous: call.starts > 1 } : { call: String(raw), seen: false };
		});
	}

	/** Forwards one tool call and returns the hub's text unchanged (EXT req. 18). */
	async tool(name: string, ms: number, callId: string, params: Json, ctx: Ctx): Promise<Json> {
		if (!this.main(ctx)) throw new Error(`${name} is not available in this session.`);
		const call = { session: this.session, agent: ctx?.agent?.id, call: callId, tool: name, input: params };
		const answer = await this.request("bridge.tool", { ...call, evidence: this.evidence(params) }, ms);
		if (!answer) throw new Error(`Cataly did not answer in time: ${UNSURE}`);
		const text = String(answer.text ?? "");
		if (answer.error) throw new Error(text);
		return { content: [{ type: "text", text }] };
	}

	/** Stop gate (EXT req. 41): continue once with the hub's reminder; otherwise stop. */
	async stop(ctx: Ctx): Promise<Json | undefined> {
		if (!this.main(ctx) || this.role !== "lead") return undefined;
		const answer = await this.request("bridge.stop", { session: this.session, answered: this.lastAnswer }, STOP_MS);
		this.lastAnswer = false;
		const context = answer?.context;
		return typeof context === "string" && context ? { continue: true, additionalContext: context } : undefined;
	}

	async shutdown(ctx: Ctx): Promise<void> {
		if (ctx?.agent?.kind === "sub") return;
		this.event("session_shutdown");
		this.shutting = true;
		if (this.wire) await Bun.sleep(SHUTDOWN_MS / 10);
		this.wire?.close();
		this.wire = undefined;
	}
}

/** The factory: two handlers, nothing else, until the hub accepts the session (EXT req. 1). */
export function bridge(pi: Pi, deps: Deps): Bridge {
	const b = new Bridge(pi, deps);
	pi.on("session_start", (_e: Json, ctx: Ctx) => b.consider(ctx));
	pi.on("session_switch", (e: Json, ctx: Ctx) => b.consider(ctx, { reason: e?.reason }));
	return b;
}

export default function cataly(pi: Pi): void {
	bridge(pi, { env: process.env, connect: unixConnect, uid: () => process.getuid?.() ?? 0 });
}
