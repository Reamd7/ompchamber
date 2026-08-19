// Worker entry that re-exports the @pierre/diffs worker module. A local entry
// is required because bundlers only resolve *relative* requests in the
// standard `new Worker(new URL(...))` syntax; this keeps the worker bundled as
// its own ES module chunk, matching the previous ?worker&url import.
import '@pierre/diffs/worker/worker.js';
