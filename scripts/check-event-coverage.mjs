#!/usr/bin/env node
// CI guard for the omp-parity event contracts (spec 05 §5.1.1, master D6).
//
// Checks (each exits 1 on failure with a precise message):
//   1. SDK union coverage — the disposition manifest's keys equal the locked
//      SDK AgentSessionEvent member set (core AgentEvent + session
//      extensions). A new SDK event without a registered disposition fails.
//   2. engine coverage — every manifest member has an explicit case in
//      engine.js #handleEngineEvent (ignore members too; no silent default).
//   3. omp registry wiring — manifest members with an omp track list their
//      public names, and every listed name exists in omp-event-registry.json.
//   4. bootstrap matrix — every durable registry entry's snapshotEndpoints
//      are covered by omp-bootstrap-matrix.json (断流不是空状态, D2).
//   5. naming discipline — every `omp.<domain>.<event>` literal in packages/
//      is registered and the `openchamber:omp` prefix never appears in code
//      (master R1). Docs under docs/omp-parity are scanned too, but lines
//      that quote deprecated names in historical/normative context
//      (mapping tables, "废止/归一/禁止" rulings) are exempt.
//
// Usage: node scripts/check-event-coverage.mjs [--sdk-dist <dir>] [--skip-name-scan]

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, '..');
const ompHostDir = path.join(repoRoot, 'packages/web/server/lib/omp-host');

const args = process.argv.slice(2);
const flag = (name) => {
  const index = args.indexOf(name);
  return index === -1 ? null : args[index + 1];
};

const failures = [];
const fail = (message) => failures.push(message);

// ---- locate SDK sources ----------------------------------------------------
const locateSdk = () => {
  const explicit = flag('--sdk-dist');
  if (explicit) return path.join(explicit, 'src/session/agent-session-events.ts');
  const direct = path.join(repoRoot, 'node_modules/@oh-my-pi/pi-coding-agent/src/session/agent-session-events.ts');
  if (fs.existsSync(direct)) return direct;
  const store = path.join(repoRoot, 'node_modules/.bun');
  if (fs.existsSync(store)) {
    for (const entry of fs.readdirSync(store)) {
      if (!entry.startsWith('@oh-my-pi+pi-coding-agent@')) continue;
      const candidate = path.join(
        store, entry, 'node_modules/@oh-my-pi/pi-coding-agent/src/session/agent-session-events.ts',
      );
      if (fs.existsSync(candidate)) return candidate;
    }
  }
  return null;
};

const locateCore = () => {
  const candidates = [
    path.join(repoRoot, 'node_modules/@oh-my-pi/pi-agent-core/dist/types/types.d.ts'),
  ];
  const store = path.join(repoRoot, 'node_modules/.bun');
  if (fs.existsSync(store)) {
    for (const entry of fs.readdirSync(store)) {
      if (entry.startsWith('@oh-my-pi+pi-agent-core@')) {
        candidates.push(path.join(store, entry, 'node_modules/@oh-my-pi/pi-agent-core/dist/types/types.d.ts'));
      }
    }
  }
  return candidates.find((candidate) => fs.existsSync(candidate)) ?? null;
};

const extractUnionMembers = (source, typeName) => {
  const anchor = source.indexOf(`type ${typeName} =`);
  if (anchor === -1) return null;
  // Type-alias unions have no wrapping braces — each member object does.
  // The alias ends at the first `;` outside any member block.
  let depth = 0;
  let end = -1;
  for (let i = source.indexOf('=', anchor); i < source.length; i += 1) {
    const ch = source[i];
    if (ch === '{') depth += 1;
    else if (ch === '}') depth -= 1;
    else if (ch === ';' && depth <= 0) {
      end = i;
      break;
    }
  }
  if (end === -1) return null;
  const body = source.slice(anchor, end);
  const members = new Set();
  const re = /type:\s*"([a-z_]+)"/g;
  let match;
  while ((match = re.exec(body))) members.add(match[1]);
  return members;
};

// ---- 1. SDK union coverage --------------------------------------------------
const sdkEventsPath = locateSdk();
if (!sdkEventsPath) {
  console.error('check-event-coverage: cannot locate @oh-my-pi/pi-coding-agent sources');
  process.exit(1);
}
const sessionEventsSource = fs.readFileSync(sdkEventsPath, 'utf8');
const sessionMembers = extractUnionMembers(sessionEventsSource, 'AgentSessionEvent');
if (!sessionMembers || sessionMembers.size === 0) {
  console.error('check-event-coverage: failed to extract AgentSessionEvent members');
  process.exit(1);
}
// The session union extends AgentEvent; core members referenced through the
// Exclude/Extract indirection must come from the core .d.ts.
const coreTypesPath = locateCore();
const coreMembers = new Set();
if (coreTypesPath) {
  const coreSource = fs.readFileSync(coreTypesPath, 'utf8');
  const extracted = extractUnionMembers(coreSource, 'AgentEvent');
  if (extracted) for (const member of extracted) coreMembers.add(member);
} else {
  fail('pi-agent-core types not found; cannot verify core AgentEvent members');
}
const sdkMembers = new Set([...coreMembers, ...sessionMembers]);

const manifest = JSON.parse(fs.readFileSync(path.join(ompHostDir, 'event-dispositions.json'), 'utf8'));
const manifestMembers = new Set(Object.keys(manifest).filter((key) => !key.startsWith('$')));
for (const member of sdkMembers) {
  if (!manifestMembers.has(member)) fail(`SDK event "${member}" has no disposition entry (register it in event-dispositions.json)`);
}
for (const member of manifestMembers) {
  if (!sdkMembers.has(member)) fail(`manifest entry "${member}" is not an SDK union member (stale after SDK downgrade?)`);
}

// ---- 2. engine case coverage ------------------------------------------------
const engineSource = fs.readFileSync(path.join(ompHostDir, 'engine.js'), 'utf8');
const handlerStart = engineSource.indexOf('#handleEngineEvent(hostSession, event)');
const switchStart = engineSource.indexOf('switch (event.type)', handlerStart);
const switchEnd = engineSource.indexOf('\n  }\n', switchStart);
const switchBody = engineSource.slice(switchStart, switchEnd);
const engineCases = new Set();
for (const match of switchBody.matchAll(/case '([a-z_]+)':/g)) engineCases.add(match[1]);
for (const member of manifestMembers) {
  if (!engineCases.has(member)) fail(`engine.js #handleEngineEvent has no explicit case for "${member}"`);
}
for (const engineCase of engineCases) {
  if (!manifestMembers.has(engineCase)) fail(`engine.js case "${engineCase}" is not in the disposition manifest`);
}

// ---- 3. omp registry wiring --------------------------------------------------
const registry = JSON.parse(fs.readFileSync(path.join(ompHostDir, 'omp-event-registry.json'), 'utf8'));
const registered = new Set(Object.keys(registry.events));
for (const [member, entry] of Object.entries(manifest)) {
  if (member.startsWith('$')) continue;
  const track = entry?.track;
  if (track === 'omp' || track === 'dual') {
    if (!Array.isArray(entry.ompEvents) || entry.ompEvents.length === 0) {
      fail(`manifest "${member}" (track=${track}) must list its ompEvents[] public names`);
      continue;
    }
    for (const name of entry.ompEvents) {
      if (!registered.has(name)) fail(`manifest "${member}" emits "${name}" which is not in omp-event-registry.json`);
    }
  }
}
// Producers for registry names live either in the manifest (05-domain) or in
// a owning chapter's domain module (02/03/04/06/08 — landed progressively).
const crossChapterEvents = new Set([
  'omp.mode.changed', 'omp.goal.updated', 'omp.plan.review_requested', 'omp.plan.updated',
  'omp.dialog.requested', 'omp.dialog.settled',
  'omp.agents.updated', 'omp.jobs.updated', 'omp.tree.updated',
  'omp.settings.updated', 'omp.queue.changed',
]);
for (const name of registered) {
  const entry = registry.events[name];
  if (entry.control || crossChapterEvents.has(name)) continue;
  const producers = Object.entries(manifest)
    .filter(([, value]) => Array.isArray(value?.ompEvents) && value.ompEvents.includes(name))
    .map(([key]) => key);
  if (producers.length === 0) fail(`registry event "${name}" has no manifest producer`);
}

// ---- 4. bootstrap matrix ------------------------------------------------------
const matrix = JSON.parse(fs.readFileSync(path.join(ompHostDir, 'omp-bootstrap-matrix.json'), 'utf8'));
const matrixEndpoints = new Set(matrix.steps.flatMap((step) => step.endpoints));
for (const [name, entry] of Object.entries(registry.events)) {
  if (!entry.durable) continue;
  for (const endpoint of entry.snapshotEndpoints ?? []) {
    if (!matrixEndpoints.has(endpoint)) {
      fail(`durable event "${name}" snapshot endpoint "${endpoint}" is missing from omp-bootstrap-matrix.json`);
    }
  }
}

// ---- 5. naming discipline ------------------------------------------------------
if (!args.includes('--skip-name-scan')) {
  const nameRe = /\bomp\.[a-z]+\.[a-z_]+\b/g;
  // Docs quote deprecated names in mapping tables and normative rulings
  const historicalLine = /禁止|废止|作废|归一|原草案|映射|→|统一命名|命名规约|沿革|零命中|已裁决|\(0\d v1\)|v1/;
  const scan = (absPath, relLabel, isDoc, isTestFile) => {
    const source = fs.readFileSync(absPath, 'utf8');
    const lines = source.split('\n');
    for (let i = 0; i < lines.length; i += 1) {
      const line = lines[i];
      if (isDoc && historicalLine.test(line)) continue;
      // Tests legitimately exercise the unknown-type forward-compat path
      // with deliberately unregistered names; only the parallel-prefix rule
      // still applies to them.
      for (const match of line.matchAll(nameRe)) {
        if (!isTestFile && !registered.has(match[0])) {
          fail(`unregistered omp event name "${match[0]}" in ${relLabel}:${i + 1}`);
        }
      }
      if (/openchamber:omp/.test(line) && (!isDoc || !historicalLine.test(line))) {
        fail(`parallel channel prefix "openchamber:omp" in ${relLabel}:${i + 1} (master R1)`);
      }
    }
  };
  const walk = (dir, relRoot, isDoc) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      if (entry.name === 'node_modules' || entry.name.startsWith('.') || entry.name === 'dist') continue;
      const full = path.join(dir, entry.name);
      const rel = path.join(relRoot, entry.name);
      if (entry.isDirectory()) {
        if (!isDoc && rel.startsWith(path.join('packages', 'web', 'server', 'lib', 'omp-host'))) continue;
        walk(full, rel, isDoc);
      } else if (/\.(?:js|mjs|cjs|ts|tsx|md|json)$/.test(entry.name)) {
        const isTestFile = /\.test\.[cm]?[jt]sx?$/.test(entry.name) || rel.includes('__tests__');
        scan(full, rel, isDoc, isTestFile);
      }
    }
  };
  walk(path.join(repoRoot, 'packages'), 'packages', false);
  walk(path.join(repoRoot, 'docs/omp-parity'), 'docs/omp-parity', true);
}

if (failures.length > 0) {
  console.error(`check-event-coverage: ${failures.length} failure(s)`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(
  `check-event-coverage: OK — ${sdkMembers.size} SDK members covered (${coreMembers.size} core + ${sessionMembers.size} session), ${engineCases.size} engine cases, ${registered.size} registered omp events`,
);
