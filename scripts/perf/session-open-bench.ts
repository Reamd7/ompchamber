#!/usr/bin/env bun
/**
 * Phase-0 omp-host memory bench (docs/plan.md §9.2) — measures the
 * allocation/retention behavior of the session-load paths the host pays for,
 * one scenario at a time, against the SAME session file repeated.
 *
 * Scenarios (`--scenario name[,name...]`, default all):
 *   open             SessionManager.open + explicit close()/releaseRetainedEntries()
 *                    (the parse cost every omp-host cold read pays today).
 *   projection       The engine's cold GET path: open → buildSessionContext
 *                    ({transcript:true}) → buildTurnStateStamper →
 *                    projectConversation → close/release, using the pure
 *                    functions from packages/web/server/lib/omp-host/projection.ts.
 *   cold-reader      SDK public read-only loader loadSessionMessagesReadOnly
 *                    (no writer, no session lock — plan §7's target path).
 *   ring             RingEventBus burst of synthetic message.part.updated-sized
 *                    events with production caps; asserts both caps hold and
 *                    reports per-emit µs plus stats().
 *   live-materialize createAgentSession + beginDispose()/dispose() round-trips.
 *                    If a headless session cannot boot without credentials or a
 *                    model registry, prints an explicit `skipped: requires …`
 *                    row — never a fake zero.
 *
 * Every scenario records three intervals from process.memoryUsage()
 * (heapUsed/external/rss; declared estimates, not heap proofs):
 *   baseline     after Bun.gc(true), before the timed work
 *   steady       after the first iteration (rows 2..n are steady-state)
 *   quiescence   after the final close/release + Bun.gc(true) + one settle
 * RSS that does not return to baseline is reported as `rssHighWaterMB` —
 * allocator high-water, NOT evidence of a leak and NOT proof of release
 * (plan §2.2: only retained heap + container counts can speak to leaks).
 *
 * Usage:
 *   bun scripts/perf/session-open-bench.ts \
 *     [--root PATH] [--file PATH] [--repeat N] \
 *     [--scenario open,projection,cold-reader,ring,live-materialize|all] \
 *     [--ring-events N] [--synthetic[=MB]]
 *
 * File selection: --file forces one file; otherwise the largest .jsonl under
 * --root (default ~/.omp/agent/sessions; largest-first discovery sort) is
 * repeated. With no session files available, --synthetic
 * generates one via the SDK's own public append API (labeled `synthetic` in
 * every output — a synthetic fixture measures mechanics, not production scale).
 *
 * Results land in artifacts/session-open-bench/{results.jsonl,summary.json}
 * (gitignored): one row per iteration (ring: per bucket of emits) and one
 * summary block per scenario. Warmup opens are untimed and explicitly
 * close()/releaseRetainedEntries() — no manager outlives its scenario.
 */
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { SessionManager } from "@oh-my-pi/pi-coding-agent/session/session-manager";
import { buildTurnStateStamper, projectConversation } from "../../packages/web/server/lib/omp-host/projection";
import { RingEventBus } from "../../packages/web/server/lib/omp-host/events";

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

function argValue(name: string): string | undefined {
	const i = process.argv.indexOf(name);
	if (i >= 0) return process.argv[i + 1];
	const prefix = `${name}=`;
	const inline = process.argv.find((a) => a.startsWith(prefix));
	return inline?.slice(prefix.length);
}

const SCENARIOS = ["open", "projection", "cold-reader", "ring", "live-materialize"] as const;
type ScenarioName = (typeof SCENARIOS)[number];

const scenarioArg = argValue("--scenario") ?? "all";
const scenarios: ScenarioName[] =
	scenarioArg === "all" ? [...SCENARIOS] : (scenarioArg.split(",").map((s) => s.trim()) as ScenarioName[]);
const unknown = scenarios.filter((s) => !SCENARIOS.includes(s));
if (unknown.length > 0) throw new Error(`unknown --scenario: ${unknown.join(", ")} (valid: ${SCENARIOS.join(", ")}, all)`);

const root = argValue("--root") ?? path.join(os.homedir(), ".omp", "agent", "sessions");
const fileArg = argValue("--file");
const repeat = Math.max(1, Number(argValue("--repeat") ?? 20) || 20);
const ringEvents = Math.max(1, Number(argValue("--ring-events") ?? 10000) || 10000);
const syntheticArg = argValue("--synthetic");
const syntheticMB = syntheticArg !== undefined ? Math.max(1, Number(syntheticArg) || 2) : undefined;

const FILE_SCENARIOS: ScenarioName[] = ["open", "projection", "cold-reader"];

/** mkdtemp dirs are bench scratch; sweep them on exit (sync exit hook). */
const tempDirs: string[] = [];
function tmpdir(prefix: string): string {
	const dir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
	tempDirs.push(dir);
	return dir;
}
process.on("exit", () => {
	for (const dir of tempDirs) fs.rmSync(dir, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------
// Measurement helpers
// ---------------------------------------------------------------------------

interface MemSnapshot {
	heapUsedMB: number;
	externalMB: number;
	rssMB: number;
}

function memSnapshot(): MemSnapshot {
	Bun.gc(true);
	const m = process.memoryUsage();
	return {
		heapUsedMB: +(m.heapUsed / 1048576).toFixed(3),
		externalMB: +(m.external / 1048576).toFixed(3),
		rssMB: +(m.rss / 1048576).toFixed(1),
	};
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Quiescence: explicit release is done, so full-GC → one macrotask settle → full-GC. */
async function quiesce(): Promise<MemSnapshot> {
	Bun.gc(true);
	await sleep(50);
	return memSnapshot();
}

interface Row {
	scenario: ScenarioName;
	iter: number;
	ms: number;
	count: number;
	/** Post-iteration Bun.gc snapshot; absent when a per-row full GC would distort the scenario (ring buckets). */
	heapUsedMB?: number;
	externalMB?: number;
	rssMB?: number;
	error?: string;
}

interface ScenarioSummary {
	scenario: ScenarioName;
	target?: string;
	iterations: number;
	failures: number;
	skipped: boolean;
	skipReason?: string;
	ms?: { median: number; p95: number; min: number; max: number };
	countMedian?: number;
	intervals?: { baseline: MemSnapshot; steady: MemSnapshot; quiescence: MemSnapshot };
	heapReturnedMB?: number;
	/** RSS non-return vs baseline. Allocator high-water — NOT leak evidence (plan §2.2). */
	rssHighWaterMB?: number;
	ring?: unknown;
	notes: string[];
}

const pick = (sorted: number[], q: number) => sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))] ?? 0;
const median = (ns: number[]) => pick([...ns].sort((a, b) => a - b), 0.5);
const p95 = (ns: number[]) => pick([...ns].sort((a, b) => a - b), 0.95);

function summarizeRows(scenario: ScenarioName, rows: Row[], intervals: { baseline: MemSnapshot; steady: MemSnapshot; quiescence: MemSnapshot }, extra: Partial<ScenarioSummary> = {}): ScenarioSummary {
	const ok = rows.filter((r) => !r.error);
	const ms = ok.map((r) => r.ms);
	const counts = ok.map((r) => r.count).sort((a, b) => a - b);
	return {
		scenario,
		iterations: rows.length,
		failures: rows.length - ok.length,
		skipped: false,
		ms: { median: +median(ms).toFixed(1), p95: +p95(ms).toFixed(1), min: +(ms.length ? Math.min(...ms) : 0).toFixed(1), max: +(ms.length ? Math.max(...ms) : 0).toFixed(1) },
		countMedian: counts.length ? counts[Math.floor(counts.length / 2)] : undefined,
		intervals,
		heapReturnedMB: +(intervals.baseline.heapUsedMB - intervals.quiescence.heapUsedMB).toFixed(3),
		rssHighWaterMB: +(intervals.quiescence.rssMB - intervals.baseline.rssMB).toFixed(1),
		...extra,
		notes: [
			...(extra.notes ?? []),
			"heapReturnedMB = baseline − quiescence heapUsed (negative ⇒ net retained across the scenario); rssHighWaterMB is allocator high-water, never leak evidence",
		],
	};
}

/** Explicit, total release of a SessionManager through public API only. */
async function releaseManager(sm: SessionManager): Promise<void> {
	try {
		await sm.close();
	} catch {
		// close() failure must not skip the terminal release below.
	}
	sm.releaseRetainedEntries();
}

// ---------------------------------------------------------------------------
// Target file selection
// ---------------------------------------------------------------------------

function* walk(dir: string): Generator<string> {
	let ents: fs.Dirent[];
	try {
		ents = fs.readdirSync(dir, { withFileTypes: true });
	} catch {
		return;
	}
	for (const ent of ents) {
		const p = path.join(dir, ent.name);
		if (ent.isDirectory()) yield* walk(p);
		else if (ent.isFile() && ent.name.endsWith(".jsonl")) yield p;
	}
}

/** Deterministic synthetic transcript via the SDK's own public append API. */
async function generateSyntheticFixture(targetMB: number): Promise<string> {
	const dir = tmpdir("session-open-bench-fx-");
	const sm = SessionManager.create(dir, dir);
	const para = "bench fixture line. ".repeat(80); // ~1.8 KiB
	const base = Date.now() - 3600_000;
	let written = 0;
	let turn = 0;
	while (written < targetMB * 1048576) {
		sm.appendMessage({ role: "user", content: `bench turn ${turn}\n${para}`, timestamp: base + turn * 2000 });
		sm.appendMessage({
			role: "assistant",
			content: [{ type: "text", text: `${para}${para}` }],
			timestamp: base + turn * 2000 + 1,
			model: "anthropic/claude-sonnet-4-5",
		} as never);
		// ~3 paras per turn (user + assistant text×2) + JSON envelope overhead.
		written += para.length * 3 + 512;
		turn += 1;
	}
	await sm.flush();
	const file = sm.getSessionFile();
	await releaseManager(sm);
	if (!file) throw new Error("synthetic fixture: getSessionFile() returned undefined");
	return file;
}

interface Target {
	file: string;
	sizeMB: number;
	synthetic: boolean;
	filesConsidered: number;
}

async function selectTarget(): Promise<Target | null> {
	if (fileArg) {
		if (!fs.existsSync(fileArg)) throw new Error(`--file not found: ${fileArg}`);
		return { file: path.resolve(fileArg), sizeMB: +(fs.statSync(fileArg).size / 1048576).toFixed(2), synthetic: false, filesConsidered: 1 };
	}
	if (syntheticMB !== undefined) {
		const file = await generateSyntheticFixture(syntheticMB);
		return { file, sizeMB: +(fs.statSync(file).size / 1048576).toFixed(2), synthetic: true, filesConsidered: 1 };
	}
	const files = [...walk(root)]
		.map((f) => ({ f, size: fs.statSync(f).size }))
		.sort((a, b) => b.size - a.size);
	if (files.length === 0) {
		throw new Error(`no .jsonl files under ${root} — pass --file PATH or --synthetic[=MB]`);
	}
	const f = files[0]!.f;
	return { file: f, sizeMB: +(files[0]!.size / 1048576).toFixed(2), synthetic: false, filesConsidered: files.length };
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

async function runOpen(target: string): Promise<{ rows: Row[]; summary: ScenarioSummary }> {
	// Untimed warmup: module init + first-parse JIT, explicitly released.
	{
		const sm = await SessionManager.open(target);
		sm.getEntries().length;
		await releaseManager(sm);
	}
	const baseline = memSnapshot();
	const rows: Row[] = [];
	let steady: MemSnapshot | undefined;
	for (let i = 1; i <= repeat; i++) {
		const t0 = performance.now();
		let count = -1;
		let error: string | undefined;
		try {
			const sm = await SessionManager.open(target);
			try {
				count = sm.getEntries().length;
			} finally {
				await releaseManager(sm);
			}
		} catch (e) {
			error = String(e).slice(0, 200);
		}
		// ms is computed before memSnapshot() — the snapshot runs a synchronous
		// Bun.gc(true) whose pause must not pollute the timed operation.
		const ms = +(performance.now() - t0).toFixed(1);
		const snap = memSnapshot();
		if (i === 1) steady = snap;
		const row: Row = { scenario: "open", iter: i, ms, count, heapUsedMB: snap.heapUsedMB, externalMB: snap.externalMB, rssMB: snap.rssMB };
		if (error) row.error = error;
		rows.push(row);
		console.log(`  [open ${i}/${repeat}] ${count}e ${ms}ms heap=${snap.heapUsedMB}MB${error ? ` ERR ${error}` : ""}`);
	}
	const quiescence = await quiesce();
	return {
		rows,
		summary: summarizeRows("open", rows, { baseline: baseline, steady: steady!, quiescence }, {
			target,
			notes: ["ms covers open + getEntries + close() + releaseRetainedEntries()", "count = entries mirrored by the manager (#entries full materialization)"],
		}),
	};
}

async function runProjection(target: string): Promise<{ rows: Row[]; summary: ScenarioSummary }> {
	const sessionID = "bench-session";
	{
		const sm = await SessionManager.open(target);
		try {
			sm.buildSessionContext({ transcript: true });
		} finally {
			await releaseManager(sm);
		}
	}
	const baseline = memSnapshot();
	const rows: Row[] = [];
	let steady: MemSnapshot | undefined;
	for (let i = 1; i <= repeat; i++) {
		const t0 = performance.now();
		let count = -1;
		let parts = -1;
		let error: string | undefined;
		try {
			const sm = await SessionManager.open(target);
			try {
				const context = sm.buildSessionContext({ transcript: true });
				const fileMessages = context.messages ?? [];
				const entries = sm.getEntries() ?? [];
				const turnStateFor = buildTurnStateStamper(entries, {});
				const projected = projectConversation(fileMessages, { sessionID, directory: path.dirname(target), turnStateFor });
				count = projected.length;
				parts = projected.reduce((s, m) => s + m.parts.length, 0);
			} finally {
				await releaseManager(sm);
			}
		} catch (e) {
			error = String(e).slice(0, 200);
		}
		// ms is computed before memSnapshot() — the snapshot runs a synchronous
		// Bun.gc(true) whose pause must not pollute the timed operation.
		const ms = +(performance.now() - t0).toFixed(1);
		const snap = memSnapshot();
		if (i === 1) steady = snap;
		const row: Row = { scenario: "projection", iter: i, ms, count, heapUsedMB: snap.heapUsedMB, externalMB: snap.externalMB, rssMB: snap.rssMB };
		if (error) row.error = error;
		rows.push(row);
		console.log(`  [projection ${i}/${repeat}] ${count}msg/${parts}parts ${ms}ms heap=${snap.heapUsedMB}MB${error ? ` ERR ${error}` : ""}`);
	}
	const quiescence = await quiesce();
	return {
		rows,
		summary: summarizeRows("projection", rows, { baseline: baseline, steady: steady!, quiescence }, {
			target,
			notes: [
				"mirrors engine #projectedMessages cold path: open → buildSessionContext({transcript:true}) → buildTurnStateStamper → projectConversation → close/release",
				"ms includes the manager open + close (the production GET pays both)",
			],
		}),
	};
}

async function runColdReader(target: string): Promise<{ rows: Row[]; summary: ScenarioSummary }> {
	const { loadSessionMessagesReadOnly } = await import("@oh-my-pi/pi-coding-agent");
	// Untimed warmup (module init paid before baseline so intervals measure workload).
	{
		const msgs = await loadSessionMessagesReadOnly(target);
		msgs.length;
	}
	const baseline = memSnapshot();
	const rows: Row[] = [];
	let steady: MemSnapshot | undefined;
	for (let i = 1; i <= repeat; i++) {
		const t0 = performance.now();
		let count = -1;
		let error: string | undefined;
		try {
			const msgs = await loadSessionMessagesReadOnly(target);
			count = msgs.length;
		} catch (e) {
			error = String(e).slice(0, 200);
		}
		// ms is computed before memSnapshot() — the snapshot runs a synchronous
		// Bun.gc(true) whose pause must not pollute the timed operation.
		const ms = +(performance.now() - t0).toFixed(1);
		const snap = memSnapshot();
		if (i === 1) steady = snap;
		const row: Row = { scenario: "cold-reader", iter: i, ms, count, heapUsedMB: snap.heapUsedMB, externalMB: snap.externalMB, rssMB: snap.rssMB };
		if (error) row.error = error;
		rows.push(row);
		console.log(`  [cold-reader ${i}/${repeat}] ${count}msg ${ms}ms heap=${snap.heapUsedMB}MB${error ? ` ERR ${error}` : ""}`);
	}
	const quiescence = await quiesce();
	return {
		rows,
		summary: summarizeRows("cold-reader", rows, { baseline, steady: steady!, quiescence }, {
			target,
			notes: [
				"SDK public read-only loader: no SessionManager writer, no session lock",
				"full-materialization fallback (plan §7.1): the loader itself retains all entries + resolves blob refs — the bench only measures the caller-side cost",
			],
		}),
	};
}

function ringPayloadText(bytes: number): string {
	const unit = "message.part.updated cumulative tool output chunk. ";
	return unit.repeat(Math.ceil(bytes / unit.length));
}

async function runRing(): Promise<{ rows: Row[]; summary: ScenarioSummary }> {
	const baseline = memSnapshot();
	// The burst lives in its own closure so the bus is unreachable by the time
	// quiescence is measured (a const still in scope would pin it).
	const burst = () => {
		const bus = new RingEventBus(); // production wire caps: capacity 2048, maxBytes 8 MiB, maxEventBytes 2 MiB
		const directory = "bench-dir";
		// Sized like a streaming message.part.updated carrying cumulative tool
		// output (projection.ts WirePartUpdatedProperties: {sessionID, part, time}).
		// A fresh payload per emit: sharing one object would let every retained
		// ring entry point at the same bytes and undercount retained heap.
		const perEmitUs: number[] = [];
		const buckets = Math.min(100, ringEvents);
		const perBucket = Math.ceil(ringEvents / buckets);
		const rows: Row[] = [];
		let steady: MemSnapshot | undefined;
		for (let b = 1; b <= buckets; b++) {
			const t0 = performance.now();
			for (let j = 0; j < perBucket; j++) {
				const e0 = performance.now();
				bus.emit("message.part.updated", { sessionID: "bench-session", part: { type: "text", id: `prt_bench_${b}_${j}`, text: ringPayloadText(4096) }, time: 1 }, directory);
				perEmitUs.push((performance.now() - e0) * 1000);
			}
			const ms = +(performance.now() - t0).toFixed(1);
			if (b === 1) steady = memSnapshot();
			rows.push({ scenario: "ring", iter: b, ms, count: perBucket });
		}
		// Over-budget burst: single events above maxEventBytes must skip the ring
		// (never truncated) and become holes. Assert that explicitly.
		const statsBefore = bus.stats();
		const bigText = ringPayloadText(bus.maxEventBytes + 65536);
		const overBudgetCount = 5;
		let overBudgetMs = 0;
		for (let j = 0; j < overBudgetCount; j++) {
			const t0 = performance.now();
			bus.emit("message.part.updated", { sessionID: "bench-session", part: { type: "text", id: `prt_big_${j}`, text: bigText } }, directory);
			overBudgetMs += performance.now() - t0;
		}
		const stats = bus.stats();
		const capsHold = stats.retainedEntries <= stats.capacity && stats.retainedBytes <= stats.maxBytes;
		const overBudgetSkipped = stats.overBudgetEvents >= overBudgetCount && stats.retainedEntries === statsBefore.retainedEntries;
		// `bus` is deliberately NOT returned: once burst() returns it is
		// unreachable, so quiescence measures a truly dropped bus.
		return { stats, capsHold, overBudgetSkipped, overBudgetCount, overBudgetMs, perEmitUs, rows, steady: steady!, perBucket, buckets };
	};
	const r = burst();
	if (!r.capsHold) throw new Error(`ring caps violated: retainedEntries=${r.stats.retainedEntries}/${r.stats.capacity} retainedBytes=${r.stats.retainedBytes}/${r.stats.maxBytes}`);
	if (!r.overBudgetSkipped) throw new Error(`over-budget events entered the ring or were not counted (overBudgetEvents=${r.stats.overBudgetEvents}, retained before/after not equal)`);
	// (The bus is already unreachable — see burst()'s return.)
	const quiescence = await quiesce();
	const sortedUs = [...r.perEmitUs].sort((a, b) => a - b);
	const summary = summarizeRows("ring", r.rows, { baseline, steady: r.steady, quiescence }, {
		notes: [
			`emitted ${r.perBucket * r.buckets} durable message.part.updated-sized events (~4 KiB payload each) + ${r.overBudgetCount} over-budget (> maxEventBytes ${r.stats.maxEventBytes} B) events`,
			`caps asserted: retainedEntries<=${r.stats.capacity}, retainedBytes<=${r.stats.maxBytes}; over-budget events skipped to holes: ${r.overBudgetSkipped}`,
			"rows are buckets of emits; per-emit µs is payload construction + the emit() call (envelope + size estimate + eviction), not subscriber delivery",
			"retainedBytes is a declared serialized-size estimate (UTF-16 units), not a JS-heap measure",
		],
		ring: {
			events: r.perBucket * r.buckets,
			perEmitUs: { median: +pick(sortedUs, 0.5).toFixed(2), p95: +pick(sortedUs, 0.95).toFixed(2), max: +pick(sortedUs, 1).toFixed(2) },
			overBudget: { count: r.overBudgetCount, totalMs: +r.overBudgetMs.toFixed(1), skippedFromRing: r.overBudgetSkipped },
			stats: r.stats,
		},
	});
	return { rows: r.rows, summary };
}

async function runLiveMaterialize(): Promise<{ rows: Row[]; summary: ScenarioSummary }> {
	const { createAgentSession } = await import("@oh-my-pi/pi-coding-agent");
	const cwd = tmpdir("session-open-bench-live-cwd-");
	const agentDir = tmpdir("session-open-bench-live-agentdir-");
	const sessionOpts = () => ({
		cwd,
		agentDir,
		sessionManager: SessionManager.inMemory(cwd),
		disableExtensionDiscovery: true,
		enableMCP: false,
		enableLsp: false,
		skipPythonPreflight: true,
		hasUI: false,
		systemPrompt: ["session-open-bench live-materialize probe"],
	});
	// Probe: can a session boot headlessly here at all? If model boot needs
	// credentials/a registry this bench does not have, say so — never fake it.
	const baseline = memSnapshot();
	try {
		// Self-test affordance: BENCH_FORCE_SKIP_LIVE=1 forces the skip path so
		// the honest-absence row can be verified on machines where boot works.
		if (process.env.BENCH_FORCE_SKIP_LIVE === "1") throw new Error("forced skip (BENCH_FORCE_SKIP_LIVE=1)");
		const probe = await createAgentSession(sessionOpts());
		probe.session.beginDispose();
		await probe.session.dispose({ drainTimeoutMs: 15000 });
		if (!probe.session.isDisposed) throw new Error("probe dispose() resolved but isDisposed is false");
	} catch (e) {
		const reason = String(e).slice(0, 300);
		console.log(`  [live-materialize] skipped: requires headless model boot — ${reason}`);
		return {
			rows: [],
			summary: {
				scenario: "live-materialize",
				iterations: 0,
				failures: 0,
				skipped: true,
				skipReason: `headless AgentSession boot unavailable in this environment: ${reason}`,
				// No ms block on a skipped scenario: zeros would read as measurements.
				notes: ["skipped rows are honest absence: createAgentSession could not boot without credentials/model registry", "re-run on a machine with a resolvable model or pass options via code to measure"],
			},
		};
	}
	const rows: Row[] = [];
	let steady: MemSnapshot | undefined;
	for (let i = 1; i <= repeat; i++) {
		const t0 = performance.now();
		let error: string | undefined;
		let disposed = false;
		try {
			const { session } = await createAgentSession(sessionOpts());
			try {
				session.beginDispose(); // public; must precede the first await after creation
				await session.dispose({ drainTimeoutMs: 15000 });
				disposed = session.isDisposed;
				if (!disposed) throw new Error("dispose() resolved but isDisposed is false");
			} finally {
				if (!disposed) {
					session.beginDispose();
					await session.dispose({ drainTimeoutMs: 15000 }).catch(() => undefined);
				}
			}
		} catch (e) {
			error = String(e).slice(0, 200);
		}
		// ms is computed before memSnapshot() — the snapshot runs a synchronous
		// Bun.gc(true) whose pause must not pollute the timed operation.
		const ms = +(performance.now() - t0).toFixed(1);
		const snap = memSnapshot();
		if (i === 1) steady = snap;
		const row: Row = { scenario: "live-materialize", iter: i, ms, count: disposed ? 1 : 0, heapUsedMB: snap.heapUsedMB, externalMB: snap.externalMB, rssMB: snap.rssMB };
		if (error) row.error = error;
		rows.push(row);
		console.log(`  [live-materialize ${i}/${repeat}] ${ms}ms disposed=${disposed} heap=${snap.heapUsedMB}MB${error ? ` ERR ${error}` : ""}`);
	}
	const quiescence = await quiesce();
	return {
		rows,
		summary: summarizeRows("live-materialize", rows, { baseline, steady: steady!, quiescence }, {
			notes: [
				"createAgentSession with in-memory SessionManager (no file IO), MCP/LSP/extensions/preflight disabled — measures agent + session boot/dispose, not JSONL parse",
				"ms covers create → beginDispose() → dispose({drainTimeoutMs:15000})",
				"cross-iteration heap growth here is createAgentSession/dispose retention (module singletons, registries), the phase-2 question",
			],
		}),
	};
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

const startedAt = new Date().toISOString();
const needFile = scenarios.some((s) => FILE_SCENARIOS.includes(s));
const target = needFile ? await selectTarget() : null;
if (target) {
	console.log(`target: ${target.file} (${target.sizeMB}MB${target.synthetic ? ", SYNTHETIC fixture" : ""}) — repeated ${repeat}x per scenario`);
}

const allRows: Row[] = [];
const summaries: ScenarioSummary[] = [];
for (const scenario of scenarios) {
	console.log(`\n=== scenario: ${scenario} ===`);
	let result: { rows: Row[]; summary: ScenarioSummary };
	if (scenario === "open") result = await runOpen(target!.file);
	else if (scenario === "projection") result = await runProjection(target!.file);
	else if (scenario === "cold-reader") result = await runColdReader(target!.file);
	else if (scenario === "ring") result = await runRing();
	else result = await runLiveMaterialize();
	allRows.push(...result.rows);
	summaries.push(result.summary);
	console.log(JSON.stringify(result.summary, null, 2));
}

const okRows = allRows.filter((r) => !r.error && !("error" in r));
const summary = {
	timestamp: startedAt,
	bun: Bun.version,
	args: { root, file: fileArg, repeat, ringEvents, scenario: scenarioArg, syntheticMB },
	target: target ? { file: target.file, sizeMB: target.sizeMB, synthetic: target.synthetic, filesConsidered: target.filesConsidered } : null,
	scenarios: summaries,
	failures: allRows.length - okRows.length,
};

const outDir = path.join(import.meta.dir, "..", "..", "artifacts", "session-open-bench");
fs.mkdirSync(outDir, { recursive: true });
fs.writeFileSync(path.join(outDir, "results.jsonl"), allRows.map((r) => JSON.stringify(r)).join("\n") + (allRows.length ? "\n" : ""));
fs.writeFileSync(path.join(outDir, "summary.json"), JSON.stringify(summary, null, 2) + "\n");

console.log(`\n=== summary (${summaries.length} scenarios, ${allRows.length} rows) ===`);
for (const s of summaries) {
	console.log(
		`${s.scenario}: ${s.skipped ? `SKIPPED (${s.skipReason})` : `${s.iterations} iters, median ${s.ms.median}ms, heapReturned ${s.heapReturnedMB}MB, rssHighWater ${s.rssHighWaterMB}MB`}`,
	);
}
console.log(`results: ${path.join(outDir, "results.jsonl")}`);
console.log(`summary: ${path.join(outDir, "summary.json")}`);
for (const r of allRows.filter((x) => x.error)) console.log(`FAIL ${r.scenario} #${r.iter}: ${r.error}`);
