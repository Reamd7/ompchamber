#!/usr/bin/env node
/**
 * Phase-0 process-tree sampler (docs/plan.md 阶段0 / §9) — answers the one
 * question RSS curves cannot: do the target's child processes (workers, tool
 * subprocesses, detached runs) return to baseline after spawn/disconnect/
 * shutdown, or do they accumulate? Child-process growth must be proven by
 * counting the descendant tree, never inferred from memory numbers.
 *
 * Samples the descendant tree of a target pid at --interval ms until
 * --duration ms elapses or SIGINT arrives, emitting one JSONL row per sample:
 *   { t, totalChildren, byName: {name: count}, depth }
 * and a final summary line:
 *   { summary: true, backend, targetPid, samples, maxChildren, finalChildren, ... }
 * Rows print to stdout; --out additionally persists them under
 * artifacts/process-tree/ (gitignored).
 *
 * Backends, per platform, no new dependencies:
 *   win32   PowerShell `Get-CimInstance Win32_Process` (ProcessId,
 *           ParentProcessId, Name, CommandLine) — one powershell per sample;
 *           falls back to wmic, then to tasklist (flat: no PPID, so
 *           totalChildren/depth are null and the summary says so — a flat
 *           count is reported as flat, never passed off as a tree).
 *   posix   `ps -eo pid=,ppid=,comm=,args=` (comm containing spaces, e.g.
 *           macOS app bundles, counts under its first token — documented
 *           approximation).
 *
 * PPID caveat: parent-pid reuse can graft unrelated orphans onto the tree.
 * The walk carries a visited set (cycle-safe) but cannot detect reuse; treat
 * single-sample spikes with CommandLine-less entries as suspect.
 *
 * Self-test: `node scripts/perf/process-tree-sample.mjs --smoke` spawns a
 * short-lived node child, asserts it appears in the tree and that the count
 * returns to baseline after exit, and exits non-zero on any failure.
 *
 * Usage:
 *   node scripts/perf/process-tree-sample.mjs [--pid PID|self] [--interval MS]
 *       [--duration MS] [--out FILE] [--smoke]
 */
import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

function argValue(name) {
	const i = process.argv.indexOf(name);
	return i >= 0 ? process.argv[i + 1] : undefined;
}

const smoke = process.argv.includes("--smoke");
const pidArg = argValue("--pid");
const targetPid = !pidArg || pidArg === "self" ? process.pid : Number(pidArg);
if (!Number.isInteger(targetPid) || targetPid <= 0) {
	console.error(`--pid must be a positive integer or "self": ${pidArg}`);
	process.exit(2);
}
const intervalMs = Math.max(50, Number(argValue("--interval") ?? 1000) || 1000);
const durationMs = Math.max(0, Number(argValue("--duration") ?? 60000) || 0); // 0 = until SIGINT
const outArg = argValue("--out");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------------------------------------------------------------------------
// Backends: obtain {pid, ppid, name}[] for every process, one call per sample.
// ---------------------------------------------------------------------------

/** Run one command; resolves {text, probePid}. probePid identifies the probe
 * process itself so its subtree can be pruned before tree construction. */
function capture(file, args, timeoutMs = 15000) {
	return new Promise((resolve, reject) => {
		const child = spawn(file, args, { stdio: ["ignore", "pipe", "ignore"], windowsHide: true });
		let out = "";
		let settled = false;
		const timer = setTimeout(() => {
			if (!settled) {
				settled = true;
				child.kill();
				reject(new Error(`${file} timed out after ${timeoutMs}ms`));
			}
		}, timeoutMs);
		child.stdout.on("data", (d) => {
			out += d;
		});
		child.stdout.on("end", () => {
			if (!settled) {
				settled = true;
				clearTimeout(timer);
				resolve({ text: out, probePid: child.pid });
			}
		});
		child.on("error", (e) => {
			if (!settled) {
				settled = true;
				clearTimeout(timer);
				reject(e);
			}
		});
		child.on("close", (code) => {
			if (!settled) {
				settled = true;
				clearTimeout(timer);
				if (code === 0) resolve({ text: out, probePid: child.pid });
				else reject(new Error(`${file} exited ${code}`));
			}
		});
	});
}

const PS_QUERY =
	"Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name,CommandLine | ConvertTo-Json -Compress";

function parsePowerShellJson(text) {
	const data = JSON.parse(text);
	const list = Array.isArray(data) ? data : [data];
	const procs = [];
	for (const p of list) {
		const pid = Number(p.ProcessId);
		const ppid = Number(p.ParentProcessId);
		if (!Number.isInteger(pid) || !Number.isInteger(ppid)) continue;
		procs.push({ pid, ppid, name: String(p.Name ?? "") });
	}
	return procs;
}

function parseWmicCsv(text) {
	// `wmic process get ProcessId,ParentProcessId,Name /format:csv` → CSV with
	// a Node,CommandLine-free header; fields: Node,Name,ParentProcessId,ProcessId.
	const procs = [];
	for (const line of text.split(/\r?\n/)) {
		if (!line.includes(",")) continue;
		const cols = line.split(",");
		const pid = Number(cols[cols.length - 1]);
		const ppid = Number(cols[cols.length - 2]);
		if (!Number.isInteger(pid) || !Number.isInteger(ppid) || cols[0] === "Node") continue;
		procs.push({ pid, ppid, name: String(cols[1] ?? "") });
	}
	return procs;
}

function parseTasklist(text) {
	// tasklist /FO CSV /NH has NO PPID: flat process list only.
	const names = [];
	for (const line of text.split(/\r?\n/)) {
		if (!line.startsWith('"')) continue;
		names.push({ pid: -1, ppid: -1, name: line.slice(1, line.indexOf('",')) });
	}
	return names;
}

const isWindows = process.platform === "win32";
let backend = null; // remembered after first successful sample

async function sampleOnce() {
	if (isWindows) {
		for (const candidate of [
			{ label: "powershell(Get-CimInstance)", file: "powershell.exe", args: ["-NoProfile", "-NonInteractive", "-Command", PS_QUERY], parse: parsePowerShellJson },
			{ label: "pwsh(Get-CimInstance)", file: "pwsh.exe", args: ["-NoProfile", "-NonInteractive", "-Command", PS_QUERY], parse: parsePowerShellJson },
			{ label: "wmic", file: "wmic.exe", args: ["process", "get", "ProcessId,ParentProcessId,Name", "/format:csv"], parse: parseWmicCsv },
		]) {
			try {
				const { text, probePid } = await capture(candidate.file, candidate.args);
				const procs = candidate.parse(text).filter((p) => p.pid !== probePid);
				if (procs.length > 0) {
					backend = candidate.label;
					return { procs, flat: false };
				}
			} catch {
				// try next backend
			}
		}
		const { text } = await capture("tasklist.exe", ["/FO", "CSV", "/NH"]);
		const flat = parseTasklist(text);
		backend = "tasklist(flat: no PPID — not a tree)";
		return { procs: flat, flat: true };
	}
	const { text, probePid } = await capture("ps", ["-eo", "pid=,ppid=,comm=,args="]);
	const procs = [];
	for (const line of text.split("\n")) {
		const trimmed = line.trim();
		if (!trimmed) continue;
		const cols = trimmed.split(/\s+/);
		const pid = Number(cols[0]);
		const ppid = Number(cols[1]);
		if (!Number.isInteger(pid) || !Number.isInteger(ppid)) continue;
		// comm is cols[2]; names with spaces count under their first token.
		procs.push({ pid, ppid, name: String(cols[2] ?? "") });
	}
	// Prune the probe itself (and thereby its subtree) so the sampler never
	// counts its own polling process as a child of the target.
	backend = "ps";
	return { procs: procs.filter((p) => p.pid !== probePid), flat: false };
}

// ---------------------------------------------------------------------------
// Descendant tree
// ---------------------------------------------------------------------------

function descendants(procs, rootPid) {
	if (procs.length > 0 && procs[0].ppid === -1) {
		// Flat backend: honest degradation — no tree, no counts by descent.
		const byName = {};
		for (const p of procs) byName[p.name] = (byName[p.name] ?? 0) + 1;
		return { totalChildren: null, byName, depth: null, pids: new Set(), flat: true };
	}
	const childrenOf = new Map();
	for (const p of procs) {
		if (!childrenOf.has(p.ppid)) childrenOf.set(p.ppid, []);
		childrenOf.get(p.ppid).push(p);
	}
	const seen = new Set([rootPid]); // cycle guard against ppid-reuse loops
	const pids = new Set();
	const byName = {};
	let totalChildren = 0;
	let depth = 0;
	const stack = (childrenOf.get(rootPid) ?? []).map((p) => [p, 1]);
	while (stack.length > 0) {
		const [p, d] = stack.pop();
		if (seen.has(p.pid)) continue;
		seen.add(p.pid);
		pids.add(p.pid);
		totalChildren += 1;
		byName[p.name] = (byName[p.name] ?? 0) + 1;
		if (d > depth) depth = d;
		for (const c of childrenOf.get(p.pid) ?? []) stack.push([c, d + 1]);
	}
	return { totalChildren, byName, depth, pids, flat: false };
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

function outPath() {
	if (!outArg) return null;
	const artifacts = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "artifacts", "process-tree");
	return path.isAbsolute(outArg) ? outArg : path.join(artifacts, outArg);
}

// ---------------------------------------------------------------------------
// Sampling loop
// ---------------------------------------------------------------------------

let stopped = false;
// Node delivers SIGINT on Windows consoles too (emulated); on POSIX this is
// the normal Ctrl+C path. Both end the loop and still print the summary.
process.on("SIGINT", () => {
	stopped = true;
});

async function runSampler() {
	const startedAt = new Date().toISOString();
	const t0 = Date.now();
	const rows = [];
	let maxChildren = 0;
	let finalChildren = null;
	let errors = 0;
	const deadline = durationMs > 0 ? t0 + durationMs : Number.POSITIVE_INFINITY;
	while (!stopped && Date.now() < deadline) {
		const sampleStart = Date.now();
		let row;
		try {
			const { procs } = await sampleOnce();
			const d = descendants(procs, targetPid);
			row = {
				t: sampleStart - t0,
				totalChildren: d.totalChildren,
				byName: d.byName,
				depth: d.depth,
			};
			if (d.totalChildren !== null && d.totalChildren > maxChildren) maxChildren = d.totalChildren;
			finalChildren = d.totalChildren;
		} catch (e) {
			errors += 1;
			row = { t: sampleStart - t0, error: String(e).slice(0, 200) };
		}
		rows.push(row);
		process.stdout.write(JSON.stringify(row) + "\n");
		const elapsed = Date.now() - sampleStart;
		if (elapsed < intervalMs && !stopped && Date.now() < deadline) await sleep(intervalMs - elapsed);
	}
	const summary = {
		summary: true,
		startedAt,
		backend,
		targetPid,
		intervalMs,
		durationMs,
		samples: rows.length,
		sampleErrors: errors,
		maxChildren,
		finalChildren,
	};
	process.stdout.write(JSON.stringify(summary) + "\n");
	const dest = outPath();
	if (dest) {
		fs.mkdirSync(path.dirname(dest), { recursive: true });
		fs.writeFileSync(dest, rows.map((r) => JSON.stringify(r)).join("\n") + "\n" + JSON.stringify(summary) + "\n");
		process.stdout.write(`written: ${dest}\n`);
	}
	return summary;
}

// ---------------------------------------------------------------------------
// Smoke self-test: a child must appear, disappear, and return count to baseline.
// ---------------------------------------------------------------------------

async function runSmoke() {
	const failures = [];
	let baseline;
	try {
		const first = await sampleOnce();
		baseline = descendants(first.procs, process.pid);
		if (baseline.flat) {
			console.error("smoke: FAIL — backend is flat (no PPID), cannot verify tree membership");
			process.exit(1);
		}
	} catch (e) {
		console.error(`smoke: FAIL — sampling backend error: ${e}`);
		process.exit(1);
	}
	console.log(`backend=${backend} baselineChildren=${baseline.totalChildren}`);

	const child = spawn(process.execPath, ["-e", "setTimeout(() => {}, 8000)"], { stdio: "ignore" });
	const childPid = child.pid;

	// Phase 1: the child must appear in the descendant tree.
	const appearDeadline = Date.now() + 12000;
	let appeared = false;
	while (!appeared && Date.now() < appearDeadline && !stopped) {
		try {
			const { procs } = await sampleOnce();
			const d = descendants(procs, process.pid);
			appeared = d.pids.has(childPid);
		} catch (e) {
			failures.push(`appear-phase sample error: ${String(e).slice(0, 120)}`);
		}
		await sleep(300);
	}
	if (!appeared) failures.push(`child pid ${childPid} never appeared in the process tree`);
	else console.log(`child ${childPid} appeared in tree`);

	// Phase 2: wait for exit, then the count must return to baseline.
	const exitCode = await new Promise((resolve) => child.on("close", (c) => resolve(c)));
	console.log(`child exited (code ${exitCode})`);
	const settleDeadline = Date.now() + 12000;
	let settled = false;
	let lastCount = null;
	while (!settled && Date.now() < settleDeadline && !stopped) {
		try {
			const { procs } = await sampleOnce();
			const d = descendants(procs, process.pid);
			lastCount = d.totalChildren;
			settled = !d.pids.has(childPid) && d.totalChildren <= baseline.totalChildren;
		} catch (e) {
			failures.push(`settle-phase sample error: ${String(e).slice(0, 120)}`);
		}
		await sleep(300);
	}
	if (!settled) failures.push(`child count did not return to baseline (last=${lastCount}, baseline=${baseline.totalChildren})`);
	else console.log(`count returned to baseline (${baseline.totalChildren})`);

	if (failures.length > 0) {
		for (const f of failures) console.error(`smoke: FAIL — ${f}`);
		process.exit(1);
	}
	console.log("smoke: PASS");
	process.exit(0);
}

if (smoke) {
	runSmoke().catch((e) => {
		console.error(`smoke: FAIL — ${e}`);
		process.exit(1);
	});
} else {
	runSampler().catch((e) => {
		console.error(String(e));
		process.exit(1);
	});
}
