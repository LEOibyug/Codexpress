import type { ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";
import { clearInterval, clearTimeout, setInterval, setTimeout } from "node:timers";

type Model = NonNullable<ExtensionContext["model"]>;
type Selection = { model: Model; modelId: string; session: string; endpoint: string };
const STATUS = "codexpress-status";
const ACTIVE_POLL_MS = 2_000;
const IDLE_POLL_MS = 30_000;
const TIMEOUT_MS = 4_000;
const MAX_EVENT_BYTES = 16_384;

// Only the configured local gateway receives its own credential. Do not read
// credential files, environment variables, or unrelated providers' credentials.
function selection(ctx: ExtensionContext): Selection | undefined {
	const model = ctx.model;
	if (!model || model.provider.toLowerCase() !== "codexpress") return;
	const session = ctx.sessionManager.getSessionId();
	if (!session || !model.id) return;
	try {
		const url = new URL(model.baseUrl);
		if (url.protocol !== "http:" || !["localhost", "127.0.0.1", "[::1]"].includes(url.hostname)
			|| url.username || url.password || url.search || url.hash || url.pathname.replace(/\/+$/, "") !== "/v1") return;
		url.pathname = "/v1/codexpress/status";
		url.searchParams.set("model", model.id);
		url.searchParams.set("session_id", session);
		return { model, modelId: model.id, session, endpoint: url.href };
	} catch { return; }
}

function same(a: Selection | undefined, b: Selection | undefined): boolean {
	return a?.model === b?.model && a?.modelId === b?.modelId && a?.session === b?.session && a?.endpoint === b?.endpoint;
}

function object(value: unknown): Record<string, unknown> | undefined {
	return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
}

function percent(value: unknown): value is number {
	return typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 100;
}

// Unknown, incomplete, or malformed data never becomes a plausible-looking zero.
export function statusText(payload: unknown): string | undefined {
	const source = object(object(payload)?.source);
	if (!source || typeof source.id !== "string" || !source.id || typeof source.name !== "string") return;
	const name = source.name.replace(/[\u0000-\u001f\u007f-\u009f]/g, "").trim().slice(0, 80);
	if (!name) return;
	if (source.kind === "official") {
		const quota = object(source.quota);
		if (!quota) return;
		const windows = [quota.short_remaining_percent, quota.weekly_remaining_percent];
		if (windows.some(value => value != null && !percent(value))) return;
		const limits = windows.filter(percent).map(value => `${Math.round(value)}%`);
		return limits.length ? `${name} ${limits.join("|")}` : undefined;
	}
	if (source.kind === "api") {
		const balance = object(source.balance);
		if (!balance || typeof balance.amount !== "number" || !Number.isFinite(balance.amount)
			|| typeof balance.currency !== "string" || !/^[A-Za-z]{3}$/.test(balance.currency)) return;
		return `${name} ${balance.amount.toLocaleString("en-US", { minimumFractionDigits: 1, maximumFractionDigits: 1, useGrouping: false })}`;
	}
}

// Bound both cache responses and individual SSE events, including ignored events.
async function readStatus(response: Response): Promise<unknown> {
	if (!response.body) throw new Error("Status body missing");
	const reader = response.body.getReader();
	const decoder = new TextDecoder("utf-8", { fatal: true });
	let body = "";
	let bytes = 0;
	try {
		for (;;) {
			const chunk = await reader.read();
			if (chunk.done) break;
			bytes += chunk.value.byteLength;
			if (bytes > MAX_EVENT_BYTES) throw new Error("Status too large");
			body += decoder.decode(chunk.value, { stream: true });
		}
		return JSON.parse(body + decoder.decode());
	} finally {
		try { await reader.cancel(); } catch { /* The request may already be aborted. */ }
		reader.releaseLock();
	}
}

async function readEvents(response: Response, receive: (payload: unknown) => void, heartbeat: () => void) {
	if (!response.body || !response.headers.get("content-type")?.toLowerCase().startsWith("text/event-stream")) {
		throw new Error("Status stream unavailable");
	}
	const reader = response.body.getReader();
	const decoder = new TextDecoder("utf-8", { fatal: true });
	let line = "";
	let event = "";
	let data = "";
	let bytes = 0;
	let skipLF = false;
	function finishLine() {
		if (!line) {
			if (event === "status") receive(JSON.parse(data));
			event = data = "";
			bytes = 0;
		} else if (!line.startsWith(":")) {
			const colon = line.indexOf(":");
			const field = colon < 0 ? line : line.slice(0, colon);
			let value = colon < 0 ? "" : line.slice(colon + 1);
			if (value.startsWith(" ")) value = value.slice(1);
			if (field === "event") event = value;
			else if (field === "data") data += `${value}\n`;
		}
		line = "";
	}
	try {
		for (;;) {
			const chunk = await reader.read();
			if (chunk.done) throw new Error("Status stream ended");
			heartbeat();
			let start = 0;
			for (let i = 0; i < chunk.value.length; i++) {
				const byte = chunk.value[i];
				if (skipLF) {
					skipLF = false;
					if (byte === 10) { start = i + 1; continue; }
				}
				// Reserve CRLF bytes even across chunks; bare CR is conservative.
				bytes += byte === 13 ? 2 : 1;
				if (bytes > MAX_EVENT_BYTES) throw new Error("Status event too large");
				if (byte !== 10 && byte !== 13) continue;
				line += decoder.decode(chunk.value.subarray(start, i), { stream: true });
				finishLine();
				start = i + 1;
				skipLF = byte === 13;
			}
			line += decoder.decode(chunk.value.subarray(start), { stream: true });
		}
	} finally {
		try { await reader.cancel(); } catch { /* The request may already be aborted. */ }
		reader.releaseLock();
	}
}

export default function codexpressStatus(pi: ExtensionAPI) {
	// Factory-local state: child sessions must not share status, requests, or ids.
	let live: ExtensionContext | undefined;
	let observed: Selection | undefined;
	let turn: { selection: Selection; ctx: ExtensionContext } | undefined;
	let inTurn = false;
	let request: AbortController | undefined;
	let stream: AbortController | undefined;
	let interval: NodeJS.Timeout | undefined;
	let reconnect: NodeJS.Timeout | undefined;
	let reconnectAttempts = 0;
	let text: string | undefined;
	let revision = 0;
	let snapshot = 0;
	let stopped = false;
	let lastPoll = 0;

	function display(next: string | undefined) {
		if (next === text) return;
		text = next;
		live?.ui.setStatus(STATUS, next);
	}

	function invalidate() {
		revision++;
		snapshot++;
		request?.abort();
		request = undefined;
		stream?.abort();
		stream = undefined;
		clearTimeout(reconnect);
		reconnect = undefined;
		display(undefined);
	}

	function observe(): Selection | undefined {
		const current = !stopped && live?.mode === "tui" && live.hasUI ? selection(live) : undefined;
		if (!same(observed, current)) {
			invalidate();
			observed = current;
			reconnectAttempts = 0;
			lastPoll = 0;
		}
		return current;
	}

	async function refreshCache() {
		const ctx = live;
		const current = observe();
		if (!ctx || !current || request) return;
		lastPoll = Date.now();
		const controller = new AbortController();
		request = controller;
		const version = revision;
		const priorSnapshot = snapshot;
		const timeout = setTimeout(() => controller.abort(), TIMEOUT_MS);
		const stillCurrent = () => version === revision && same(current, observe()) && request === controller;
		try {
			const key = await ctx.modelRegistry.getApiKey(current.model, current.session, { signal: controller.signal });
			if (!stillCurrent() || controller.signal.aborted) return;
			if (!key) throw new Error("Status unavailable");
			const response = await fetch(current.endpoint, {
				headers: { Authorization: `Bearer ${key}`, Accept: "application/json" },
				signal: controller.signal, redirect: "error", cache: "no-store",
			});
			if (!response.ok) throw new Error("Status unavailable");
			const next = statusText(await readStatus(response));
			if (stillCurrent() && !controller.signal.aborted && snapshot === priorSnapshot) display(next);
		} catch {
			// Never emit keys, URLs, payloads, or server errors to logs or the UI.
			if (stillCurrent()) { display(undefined); stream?.abort(); }
		} finally {
			clearTimeout(timeout);
			controller.abort();
			if (request === controller) request = undefined;
		}
	}

	async function subscribe() {
		const ctx = live;
		const current = observe();
		if (!ctx || !current || stream || reconnect) return;
		const controller = new AbortController();
		stream = controller;
		const version = revision;
		const started = Date.now();
		let timeout = setTimeout(() => controller.abort(), TIMEOUT_MS);
		const stillCurrent = () => version === revision && same(current, observe()) && stream === controller;
		try {
			const key = await ctx.modelRegistry.getApiKey(current.model, current.session, { signal: controller.signal });
			if (!stillCurrent() || controller.signal.aborted) return;
			if (!key) throw new Error("Status unavailable");
			const url = new URL(current.endpoint);
			url.pathname = "/v1/codexpress/events";
			const response = await fetch(url, {
				headers: { Authorization: `Bearer ${key}`, Accept: "text/event-stream" },
				signal: controller.signal, redirect: "error", cache: "no-store",
			});
			if (!response.ok || controller.signal.aborted) throw new Error("Status unavailable");
			const heartbeat = () => {
				clearTimeout(timeout);
				timeout = setTimeout(() => controller.abort(), 45_000);
			};
			heartbeat();
			await readEvents(response, payload => {
				if (stillCurrent() && !controller.signal.aborted) {
					snapshot++;
					display(statusText(payload));
				}
			}, heartbeat);
		} catch {
			if (stillCurrent()) display(undefined);
		} finally {
			clearTimeout(timeout);
			controller.abort();
			if (stillCurrent()) {
				stream = undefined;
				if (Date.now() - started >= IDLE_POLL_MS) reconnectAttempts = 0;
				const delay = Math.min(1_000 * 2 ** Math.min(reconnectAttempts++, 5), IDLE_POLL_MS);
				reconnect = setTimeout(() => {
					reconnect = undefined;
					try { void subscribe(); } catch { invalidate(); }
				}, delay);
				void refreshCache();
			}
		}
	}

	async function refreshTurn(completed: { selection: Selection; ctx: ExtensionContext }) {
		const { selection: current, ctx } = completed;
		const controller = new AbortController();
		const timeout = setTimeout(() => controller.abort(), TIMEOUT_MS);
		try {
			// Use the captured actual request identity, never a later display selection.
			const key = await ctx.modelRegistry.getApiKey(current.model, current.session, { signal: controller.signal });
			if (!key || controller.signal.aborted) return;
			const url = new URL(current.endpoint);
			url.pathname = "/v1/codexpress/refresh";
			url.search = "";
			const response = await fetch(url, {
				method: "POST",
				headers: { Authorization: `Bearer ${key}`, "Content-Type": "application/json" },
				body: JSON.stringify({ model: current.modelId, session_id: current.session }),
				signal: controller.signal, redirect: "error", cache: "no-store",
			});
			// The gateway acknowledges immediately; it owns the read-only upstream work.
			await response.body?.cancel();
		} catch { /* Refresh failure must not block a completed conversation. */ }
		finally { clearTimeout(timeout); controller.abort(); }
	}

	function activate(ctx: ExtensionContext) {
		live = ctx;
		stopped = false;
		observe();
		if (ctx.mode !== "tui" || !ctx.hasUI) return;
		if (!interval) interval = setInterval(() => {
			try {
				observe();
				void subscribe();
				if (!stream && Date.now() - lastPoll >= (live?.isIdle() ? IDLE_POLL_MS : ACTIVE_POLL_MS)) void refreshCache();
			} catch { invalidate(); }
		}, ACTIVE_POLL_MS);
		// OMP 18.15 has no model_changed event. This zero-height guard clears
		// obsolete native footer text synchronously on normal composer redraws.
		ctx.ui.setWidget(`${STATUS}-guard`, () => ({
			render: () => { observe(); return []; },
			invalidate: () => {},
		}), { placement: "belowEditor" });
		void subscribe();
	}

	pi.on("session_start", (_event, ctx) => { activate(ctx); });
	pi.on("before_agent_start", (_event, ctx) => {
		turn = undefined;
		inTurn = true;
		activate(ctx);
	});
	pi.on("before_provider_request", (event, ctx) => {
		const current = selection(ctx);
		const payload = object(event.payload);
		if (!current || !payload) return;
		// Hook contexts identify the actual role/fallback model. Keep only the
		// last Codexpress request in this turn; do not alter model/cache-key rows.
		if (inTurn) turn = { selection: current, ctx };
		return { ...payload, codexpress_session_id: current.session };
	});
	pi.on("after_provider_response", event => {
		if (event.status < 200 || event.status >= 300) {
			invalidate();
			// Only the local stream reconnects; no upstream request is retried.
		} else void refreshCache();
	});
	pi.on("agent_end", async (_event, ctx) => {
		const completed = turn;
		turn = undefined;
		inTurn = false;
		activate(ctx);
		void refreshCache();
		// Await only the local quick acknowledgement, including in print/RPC/child
		// sessions, so shutdown cannot discard a refresh before it is submitted.
		if (completed) await refreshTurn(completed);
	});
	pi.on("session_before_switch", () => { stopped = true; invalidate(); });
	pi.on("session_before_branch", () => { stopped = true; invalidate(); });
	pi.on("session_before_tree", () => { stopped = true; invalidate(); });
	for (const event of ["session_switch", "session_branch", "session_tree"] as const) {
		pi.on(event, (_event, ctx) => { activate(ctx); });
	}
	pi.on("session_shutdown", (_event, ctx) => {
		stopped = true;
		invalidate();
		turn = undefined;
		inTurn = false;
		clearInterval(interval);
		interval = undefined;
		ctx.ui.setWidget(`${STATUS}-guard`, undefined);
		live = undefined;
	});
}
