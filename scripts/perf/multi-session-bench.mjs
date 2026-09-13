#!/usr/bin/env bun
// Multi-session cold-load memory probe (docs/plan.md §7.2/§9.2 companion).
//
// Loads N DISTINCT real transcripts twice per file:
//   streamed  readTranscriptMessagePage — the §7.2 two-pass page arm whose
//             retained state must stay bounded to the page window.
//   full      withColdManager — the pre-change open→read→release arm whose
//             retained state must also settle, but at transcript scale.
//
// For every file and arm we record heapUsed/rss immediately after the call
// (transient + retained) and after Bun.gc (retained only). If the streamed
// arm is correct, its post-GC delta stays flat across N files; linear drift
// would be a per-load leak. The full arm's post-call number shows the peak
// footprint the streamed arm exists to avoid.
//
// Usage:
//   bun scripts/perf/multi-session-bench.mjs [root] [count] [pageLimit]
// Defaults: ~/.omp/agent/sessions, 60 files (30 largest + 30 spread), page 30.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { readSessionScalars, readTranscriptMessagePage } from "../../packages/web/server/lib/omp-host/cold-transcript-page.ts";
import { withColdManager } from "../../packages/web/server/lib/omp-host/cold-reader.ts";

const root = process.argv[2] ?? path.join(os.homedir(), ".omp", "agent", "sessions");
const count = Math.max(1, Number(process.argv[3] ?? 60) || 60);
const pageLimit = Math.max(1, Number(process.argv[4] ?? 30) || 30);

const mb = (bytes) => +(bytes / 1048576).toFixed(2);
const snap = (gc) => {
  if (gc) Bun.gc(true);
  const m = process.memoryUsage();
  return { heap: m.heapUsed, rss: m.rss };
};

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
      } catch {
        // locked or vanished file — skip
      }
    }
  }
};
walk(root);
files.sort((a, b) => b.size - a.size);

// Selection: the largest halfCount plus a uniform spread through the rest.
const half = Math.floor(count / 2);
const selected = files.slice(0, Math.min(half, files.length));
const rest = files.slice(half);
const stride = Math.max(1, Math.floor(rest.length / Math.max(1, count - selected.length)));
for (let i = 0; i < rest.length && selected.length < count; i += stride) {
  selected.push(rest[i]);
}

console.log(`corpus=${files.length} selected=${selected.length} pageLimit=${pageLimit}`);
console.log(`largest=${mb(selected[0]?.size ?? 0)}MB smallest=${mb(selected[selected.length - 1]?.size ?? 0)}MB`);

const rows = [];
let nulls = 0;
let errors = 0;

for (let i = 0; i < selected.length; i++) {
  const file = selected[i];
  const row = { i, sizeMB: mb(file.size) };
  try {
    const scalars = await readSessionScalars(file.path);
    row.sessionID = scalars?.id ?? null;

    // Arm A: streamed page read.
    const aBefore = snap(true);
    const page = await readTranscriptMessagePage(file.path, {
      sessionID: row.sessionID ?? "unknown",
      limit: pageLimit,
    });
    const aAfter = snap(false);
    const aSettled = snap(true);
    row.a = {
      null: page === null,
      msgs: page?.page?.messages?.length ?? 0,
      atCallMB: mb(aAfter.heap - aBefore.heap),
      retainedMB: mb(aSettled.heap - aBefore.heap),
    };
    if (page === null) nulls += 1;

    // Arm B: full cold manager, peak sampled inside consume (entries live).
    const bBefore = snap(true);
    let bPeak = 0;
    let entries = 0;
    await withColdManager(file.path, (m) => {
      entries = m.getEntries?.()?.length ?? 0;
      bPeak = snap(false).heap - bBefore.heap;
    });
    const bSettled = snap(true);
    row.b = {
      entries,
      peakMB: mb(bPeak),
      retainedMB: mb(bSettled.heap - bBefore.heap),
    };
  } catch (error) {
    errors += 1;
    row.error = String(error?.message ?? error).slice(0, 120);
  }
  row.heapMB = mb(snap(true).heap);
  row.rssMB = mb(snap(false).rss);
  rows.push(row);
  if ((i + 1) % 10 === 0 || i === selected.length - 1) {
    console.log(`[${i + 1}/${selected.length}] heap=${row.heapMB}MB rss=${row.rssMB}MB`);
  }
}

const deltas = rows.flatMap((r) => [r.a?.retainedMB, r.b?.retainedMB]).filter((v) => typeof v === "number");
const peaks = rows.map((r) => r.b?.peakMB).filter((v) => typeof v === "number");
const first = rows.find((r) => r.heapMB !== undefined);
const last = rows[rows.length - 1];
const summary = {
  files: rows.length,
  streamedNullRate: +(nulls / Math.max(1, rows.length)).toFixed(3),
  errors,
  heapStartMB: first?.heapMB,
  heapEndMB: last?.heapMB,
  heapDriftMB: +(last.heapMB - first.heapMB).toFixed(2),
  perFileRetainedMB: {
    min: Math.min(...deltas),
    median: deltas.sort((a, b) => a - b)[Math.floor(deltas.length / 2)],
    max: Math.max(...deltas),
  },
  fullArmPeakMB: { median: peaks.sort((a, b) => a - b)[Math.floor(peaks.length / 2)], max: Math.max(...peaks) },
};
console.log(JSON.stringify({ summary }, null, 2));
for (const r of rows.filter((r) => r.error || (r.a?.retainedMB ?? 0) > 2 || (r.b?.retainedMB ?? 0) > 2)) {
  console.log(JSON.stringify(r));
}
fs.mkdirSync("artifacts", { recursive: true });
fs.writeFileSync("artifacts/multi-session-bench.json", JSON.stringify({ summary, rows }, null, 2));
