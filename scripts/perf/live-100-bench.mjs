#!/usr/bin/env bun
// Live-materialize memory probe (docs/plan.md §3/§9.2 companion): drives the
// REAL OmpHostEngine so 100 distinct transcripts sit live at once, then
// shutdown() to prove the release path.
//
// What is real: SessionManager.open on real transcripts (the entries mirror
// is the dominant per-session retention), the live registry, the event bus,
// the domain stack. What is stubbed: createAgentSession returns a fixture
// session bound to the real manager — AgentSession's own internals (tool
// state, model context) are NOT counted, so per-session numbers understate
// a fully warm agent by a constant, not by the transcript term.
//
// All writes land in a temp agentDir: copied transcripts, sidecar registry,
// settings — real user data is never touched.
//
// Usage: bun scripts/perf/live-100-bench.mjs [root] [count]
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { SessionManager } from "@oh-my-pi/pi-coding-agent/session/session-manager";

const root = process.argv[2] ?? path.join(os.homedir(), ".omp", "agent", "sessions");
const count = Math.max(1, Number(process.argv[3] ?? 100) || 100);

const mb = (bytes) => +(bytes / 1048576).toFixed(1);
const snap = (gc) => {
  if (gc) Bun.gc(true);
  const m = process.memoryUsage();
  return { heap: m.heapUsed, rss: m.rss };
};

// Collect real transcripts: largest first (worst case), unique ids.
const files = [];
const walk = (d) => {
  let ents;
  try {
    ents = fs.readdirSync(d, { withFileTypes: true });
  } catch {
    return;
  }
  for (const e of ents) {
    const p = path.join(d, e.name);
    if (e.isDirectory()) walk(p);
    else if (e.name.endsWith(".jsonl")) {
      try {
        files.push({ size: fs.statSync(p).size, path: p });
      } catch {}
    }
  }
};
walk(root);
files.sort((a, b) => b.size - a.size);

const agentDir = fs.mkdtempSync(path.join(os.tmpdir(), "omp-live-bench-"));
const fakeCwd = path.join(os.tmpdir(), "omp-live-bench-cwd");
const sessionDir = SessionManager.getDefaultSessionDir(fakeCwd, agentDir);
fs.mkdirSync(sessionDir, { recursive: true });

const selected = files.slice(0, count);
for (const f of selected) {
  fs.copyFileSync(f.path, path.join(sessionDir, path.basename(f.path)));
}
// Session ids come from the transcript header, not the filename — a file
// named `2026-08-05T…_<uuid>.jsonl` lists as just the uuid.
const listed = await SessionManager.list(undefined, sessionDir);
const idByPath = new Map(listed.map((info) => [info.path, info.id]));
console.log(`corpus=${files.length} selected=${selected.length} listed=${listed.length} agentDir=${agentDir}`);

let disposeCalls = 0;
let disposeFailures = 0;
const fakeSession = (manager) => ({
  sessionManager: manager,
  model: { provider: "bench", id: "m1" },
  messages: [],
  isStreaming: false,
  isAborting: false,
  isRetrying: false,
  isCompacting: false,
  isGeneratingHandoff: false,
  isBashRunning: false,
  isEvalRunning: false,
  hasPendingBashMessages: false,
  hasPendingPythonMessages: false,
  hasPostPromptWork: false,
  queuedMessageCount: 0,
  subscribe: () => () => {},
  hasPendingAsyncWork: () => false,
  beginDispose: () => {},
  // Real AgentSession.dispose tears its manager down; the fixture must do
  // the same or the entries mirror survives disposal.
  dispose: async () => {
    disposeCalls += 1;
    try {
      await manager.close();
    } catch {
      disposeFailures += 1;
    }
    manager.releaseRetainedEntries?.();
  },
  prompt: async () => true,
  abort: async () => {},
  maybeStartTitleGeneration: () => {},
  setModel: async () => {},
  setThinkingLevel: () => {},
});

const { OmpHostEngine } = await import("../../packages/web/server/lib/omp-host/engine.ts");
const engine = new OmpHostEngine({
  agentDir,
  createAgentSession: async (options) => ({
    session: fakeSession(options.sessionManager),
    setToolUIContext: () => {},
  }),
});

const baseline = snap(true);
console.log(`baseline heap=${mb(baseline.heap)}MB rss=${mb(baseline.rss)}MB`);

const rows = [];
let failures = 0;
for (let i = 0; i < selected.length; i++) {
  const copied = path.join(sessionDir, path.basename(selected[i].path));
  const id = idByPath.get(copied) ?? idByPath.get(path.resolve(copied)) ?? path.basename(selected[i].path, ".jsonl");
  const t0 = performance.now();
  try {
    const result = await engine.prompt({ sessionID: id, directory: fakeCwd, text: "bench-materialize" });
    if (result === null) {
      failures += 1;
      rows.push({ i, id, error: "prompt returned null (file not found by list)" });
      continue;
    }
  } catch (error) {
    failures += 1;
    const msg = String(error?.stack ?? error?.message ?? error).slice(0, 600);
    rows.push({ i, id, error: msg });
    if (failures <= 3) console.log(`FAIL ${id}: ${msg}`);
    continue;
  }
  const now = snap(true);
  rows.push({ i, id, sizeMB: mb(selected[i].size), ms: +(performance.now() - t0).toFixed(0), heapMB: mb(now.heap), rssMB: mb(now.rss) });
  if ((i + 1) % 10 === 0 || i === selected.length - 1) {
    console.log(`[${i + 1}/${selected.length}] heap=${rows[rows.length - 1].heapMB}MB rss=${rows[rows.length - 1].rssMB}MB`);
  }
}

const liveStats = engine.getStreamDiagnostics?.().dataProportional ?? null;
const afterLoad = snap(true);

// Phase 2: release everything through the real shutdown path.
await engine.shutdown();
// Freed objects need a macrotask turn before the sweep is visible in
// heapUsed — same quiescence protocol as session-open-bench.
await new Promise((r) => setTimeout(r, 100));
const afterShutdown = snap(true);
// GC-visibility probe: several settle+GC rounds — if heap keeps stepping
// down the objects were dead but unswept; a flat floor means real retention.
const gcRounds = [mb(afterShutdown.heap)];
for (let g = 0; g < 5; g++) {
  await new Promise((r) => setTimeout(r, 200));
  gcRounds.push(mb(snap(true).heap));
}
console.log(`postShutdownGC rounds: ${gcRounds.join(" -> ")}MB`);
const postShutdown = engine.getStreamDiagnostics?.().dataProportional ?? null;
console.log(`disposeCalls=${disposeCalls} disposeFailures=${disposeFailures} postShutdown=${JSON.stringify(postShutdown?.liveSessions)}`);

if (process.env.BENCH_HEAPSNAP === "1") {
  const { generateHeapSnapshotForDebugging } = await import("bun:jsc");
  Bun.gc(true);
  const snapJ = generateHeapSnapshotForDebugging();
  fs.writeFileSync("artifacts/live-100.heapsnapshot.json", JSON.stringify(snapJ));
  console.log(`heapsnapshot nodes=${snapJ.nodes?.length} written to artifacts/live-100.heapsnapshot.json`);
}

console.log(
  JSON.stringify(
    {
      baselineMB: mb(baseline.heap),
      after100LiveMB: afterLoad.heap / 1048576,
      afterShutdownMB: afterShutdown.heap / 1048576,
      liveGrowthMB: mb(afterLoad.heap - baseline.heap),
      shutdownReturnMB: mb(afterShutdown.heap - baseline.heap),
      rssAfterLoadMB: mb(afterLoad.rss),
      rssAfterShutdownMB: mb(afterShutdown.rss),
      failures,
      liveStats,
      perSessionMB: failures < rows.length ? mb((afterLoad.heap - baseline.heap) / (rows.length - failures)) : null,
    },
    null,
    2,
  ),
);
fs.mkdirSync("artifacts", { recursive: true });
fs.writeFileSync("artifacts/live-100-bench.json", JSON.stringify({ rows }, null, 2));
try {
  fs.rmSync(agentDir, { recursive: true, force: true });
} catch {
  // Open manager/watcher handles can keep the temp dir busy on Windows —
  // it is temp space, the OS reclaims it.
}
