// DOM structure dumper: a screenshot shows WHAT is on screen, this shows HOW
// to address it (tag.class[role] + rects + leaf text). Use before writing
// locators in a scenario — grep-able output beats re-shooting the same state.
// Usage: node tree.mjs [url]   (defaults to harness APP_URL)
import { chromium } from 'playwright-core';
import { APP_URL, CHROMIUM_BIN } from './harness.mjs';

const url = process.argv[2] ?? APP_URL;
const browser = await chromium.launch({ executablePath: CHROMIUM_BIN, headless: true });
const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
const page = await context.newPage();
await page.goto(url, { timeout: 60000, waitUntil: 'load' });
await page.waitForTimeout(6000);

const tree = await page.evaluate(() => {
  const lines = [];
  const walk = (el, depth) => {
    if (depth > 11) return;
    const r = el.getBoundingClientRect();
    const cls = (el.getAttribute('class') ?? '').split(' ').slice(0, 2).join('.');
    const txt = (el.children.length === 0 ? el.textContent.trim().slice(0, 30) : '');
    const clickable = el.hasAttribute('role') || el.getAttribute('tabindex') !== null || el.tagName === 'A';
    lines.push(`${'  '.repeat(depth)}${el.tagName.toLowerCase()}${cls ? '.' + cls : ''}${clickable ? '[role=' + el.getAttribute('role') + ']' : ''} [${Math.round(r.x)},${Math.round(r.y)} ${Math.round(r.width)}x${Math.round(r.height)}] ${txt}`);
    for (const c of el.children) walk(c, depth + 1);
  };
  walk(document.getElementById('root') || document.body, 0);
  return lines.slice(0, 120).join('\n');
});
console.log(tree);
await context.close();
await browser.close();
