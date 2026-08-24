// Worker entry that pulls in the @pierre/diffs worker module. A local entry is
// required because bundlers only resolve *relative* requests in worker import
// syntaxes; workerFactory.ts loads this file (inline variant) so the worker is
// embedded as a self-contained blob instead of a runtime-fetched chunk.
//
// @pierre/diffs declares `sideEffects: ["dist/components/web-components.js"]`,
// so this bare side-effect-only import tree-shakes to nothing unless the
// rsbuild config marks the module side-effectful (see the module.rules entry
// in packages/web/rsbuild.config.ts). Without that rule the emitted worker is
// empty: it spawns, answers nothing, and every diff render hangs silently
// with a blank body and no error.
import '@pierre/diffs/worker/worker.js';
