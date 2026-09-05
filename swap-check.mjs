// Temp swap: run the HEAD version of runtime.test.js against the current runtime.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';

const p = 'packages/web/server/lib/terminal/runtime.test.js';
const mine = fs.readFileSync(p, 'utf8');
const head = execFileSync('git', ['show', 'cf03beb7~1:' + p], { encoding: 'utf8', cwd: process.cwd() });
fs.writeFileSync(p, head);
try {
  const out = execFileSync('bunx', ['vitest', 'run', 'server/lib/terminal/runtime.test.js'], {
    cwd: 'packages/web', encoding: 'utf8', timeout: 180000, stdio: ['ignore', 'pipe', 'pipe'],
  });
  console.log((out.match(/Tests\s+.*$/m) || ['no match'])[0]);
} finally {
  fs.writeFileSync(p, mine);
}
