import type { ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";

type Model = NonNullable<ExtensionContext["model"]>;
type Selection = { model: Model; session: string; endpoint: string };
const STATUS = "codexpress-status";
const ACTIVE_POLL_MS = 2_000;
const IDLE_POLL_MS = 30_000;
const TIMEOUT_MS = 4_000;

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
		return { model, session, endpoint: url.href };
	} catch { return; }
}

function same(a: Selection | undefined, b: Selection | undefined): boolean {
	return a?.model === b?.model && a?.session === b?.session && a?.endpoint === b?.endpoint;
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

export default function codexpressStatus(pi: ExtensionAPI) {
	// Factory-local state: child sessions must not share status, requests, or ids.
	let live: ExtensionContext | undefined;
	let observed: Selection | undefined;
	let request: AbortController | undefined;
	let interval: ReturnType<ExtensionContext["setInterval"]> | undefined;
	let text: string | undefined;
	let revision = 0;
	let stopped = false;
	let lastPoll = 0;

	function display(next: string | undefined) {
		if (next === text) return;
		text = next;
		live?.ui.setStatus(STATUS, next);
	}

	function invalidate() {
		revision++;
		request?.abort();
		request = undefined;
		display(undefined);
	}

	function observe(): Selection | undefined {
		const current = !stopped && live ? selection(live) : undefined;
		if (!same(observed, current)) {
			invalidate();
			observed = current;
		}
		return current;
	}

	async function refresh() {
		const ctx = live;
		const current = observe();
		if (!ctx || !current || request) return;
		lastPoll = Date.now();
		const controller = new AbortController();
		request = controller;
		const version = revision;
		const timeout = ctx.setTimeout(() => controller.abort(), TIMEOUT_MS);
		const stillCurrent = () => !controller.signal.aborted && version === revision
			&& same(current, observe()) && request === controller;
		try {
			const key = await ctx.modelRegistry.getApiKey(current.model, current.session, { signal: controller.signal });
			if (!stillCurrent() || !key) { if (stillCurrent()) display(undefined); return; }
			const response = await fetch(current.endpoint, {
				headers: { Authorization: `Bearer ${key}`, Accept: "application/json" },
				signal: controller.signal,
				redirect: "error",
				cache: "no-store",
			});
			if (!response.ok) throw new Error("Status unavailable");
			if (!response.body) throw new Error("Status body missing");
			const reader = response.body.getReader();
			const decoder = new TextDecoder();
			let body = "";
			let bytes = 0;
			try {
				for (;;) {
					const chunk = await reader.read();
					if (chunk.done) break;
					bytes += chunk.value.byteLength;
					if (bytes > 16_384) {
						await reader.cancel();
						throw new Error("Status too large");
					}
					body += decoder.decode(chunk.value, { stream: true });
				}
				body += decoder.decode();
			} finally { reader.releaseLock(); }
			const next = statusText(JSON.parse(body));
			if (stillCurrent()) display(next);
		} catch {
			// No key, URL, payload, or server error is emitted to logs or the UI.
			if (version === revision && same(current, observe())) display(undefined);
		} finally {
			ctx.clearTimer(timeout);
			if (request === controller) request = undefined;
		}
	}


	function activate(ctx: ExtensionContext) {
		live = ctx;
		stopped = false;
		observe();
		if (ctx.mode !== "tui" || !ctx.hasUI) return;
		if (!interval) interval = ctx.setInterval(() => {
			observe();
			if (Date.now() - lastPoll >= (live?.isIdle() ? IDLE_POLL_MS : ACTIVE_POLL_MS)) void refresh();
		}, ACTIVE_POLL_MS);
		// OMP 18.15 has no model_changed extension event. This zero-height
		// component observes the live model on normal composer redraws, clearing
		// obsolete footer text synchronously. It never replaces the footer or
		// adds a visible widget, and display() is idempotent to avoid redraw loops.
		ctx.ui.setWidget(`${STATUS}-guard`, () => ({
			render: () => { observe(); return []; },
			invalidate: () => {},
		}), { placement: "belowEditor" });
	}

	pi.on("session_start", (_event, ctx) => {
		activate(ctx);
		if (ctx.mode === "tui") void refresh();
	});
	pi.on("before_agent_start", (_event, ctx) => {
		activate(ctx);
		invalidate();
	});
	pi.on("before_provider_request", (event, ctx) => {
		const current = selection(ctx);
		const payload = object(event.payload);
		if (!current || !payload) return;
		// A supported payload hook runs for the actual request model, including
		// role/fallback models. The authenticated local adapter consumes this
		// field before forwarding, leaving prompt-cache keys and model rows intact.
		return { ...payload, codexpress_session_id: current.session };
	});
	pi.on("after_provider_response", (event, ctx) => {
		if (event.status < 200 || event.status >= 300) invalidate();
		else if (ctx.mode === "tui") void refresh();
	});
	pi.on("agent_end", (_event, ctx) => {
		activate(ctx);
		invalidate();
		if (ctx.mode === "tui") void refresh();
	});
	pi.on("session_before_switch", () => { stopped = true; invalidate(); });
	pi.on("session_before_branch", () => { stopped = true; invalidate(); });
	pi.on("session_before_tree", () => { stopped = true; invalidate(); });
	for (const event of ["session_switch", "session_branch", "session_tree"] as const) {
		pi.on(event, (_event, ctx) => {
			activate(ctx);
			if (ctx.mode === "tui") void refresh();
		});
	}
	pi.on("session_shutdown", (_event, ctx) => {
		stopped = true;
		invalidate();
		if (interval) ctx.clearTimer(interval);
		interval = undefined;
		ctx.ui.setWidget(`${STATUS}-guard`, undefined);
		live = undefined;
	});
}
