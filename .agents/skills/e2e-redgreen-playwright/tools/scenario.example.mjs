// Example Red/Green scenario — the fingerprint-assertion pattern in miniature.
// A custom command planted in the scratch project (.opencode/command/e2e-echo.md)
// expands to a marker line that embeds the arguments, so ONE regex proves
// whether args survived the round trip. Structure every scenario the same way:
// bootstrap -> numbered shots -> settle() -> verdict().
// Run:     node scenario.example.mjs
// Green:   RUN_NAME=green-slash node scenario.example.mjs   (same script, new artifacts dir)
import { newRun, verdict } from './harness.mjs';
import { bootstrapApp, composer, transcriptText } from './bootstrap.mjs';

const run = await newRun(process.env.RUN_NAME ?? 'red-slash');
const { page } = run;
try {
  await bootstrapApp(page);
  await run.shot('01-draft', 'draft session in scratch project');

  const c = composer(page);
  await c.click();
  await page.keyboard.type('/e2e-echo hello world', { delay: 25 });
  await page.waitForTimeout(1000);
  await run.shot('02-typed', 'typed /e2e-echo hello world');

  await page.keyboard.press('Enter');
  run.log('submitted');
  await page.waitForTimeout(4000);
  await run.shot('03-sent', 'sent — optimistic user message');

  const txt = await run.settle(() => transcriptText(page));
  await run.shot('04-settled', 'turn settled');

  const hasMarker = /E2E-ECHO-OK/i.test(txt);
  const hasArgs = /hello world/.test(txt);
  run.log('marker:', hasMarker, '| args:', hasArgs);
  run.log('transcript head:', txt.slice(0, 700).replace(/\n+/g, ' | '));
  // Red observed on 2026-10-18: model received the bare command name and
  // guessed the marker from files with EMPTY args ("E2E-ECHO-OK []") — the
  // criterion is marker WITH args, not marker alone.
  verdict('SLASH', hasMarker && hasArgs, hasMarker && hasArgs ? 'expanded with args' : 'args dropped / literal command name');
} finally {
  await run.finish();
}
