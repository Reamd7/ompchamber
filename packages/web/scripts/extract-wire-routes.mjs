// Dev tooling: extract the consumed route map from the vendored generated client.
// Not shipped; run with: bun scripts/extract-wire-routes.mjs (from packages/web)
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const src = fs.readFileSync(
  path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../ui/src/lib/opencode/wire/gen/sdk.gen.js'),
  'utf8',
);
const parts = src.split('export class ').slice(1);
for (const part of parts) {
  const cls = part.slice(0, part.indexOf(' '));
  const methodRe = /(?:async )?(\w+)\(parameters, options\) \{([\s\S]*?)\n    \}/g;
  let m;
  while ((m = methodRe.exec(part))) {
    const verb = m[2].match(/\)\.(\w+)\(\{/);
    const url = m[2].match(/url: "([^"]+)"/);
    if (verb && url) console.log(cls + '.' + m[1] + ' ' + verb[1].toUpperCase() + ' ' + url[1]);
  }
}
