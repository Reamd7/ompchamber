import PierreDiffWorker from './pierre-diff.worker.ts?worker&inline';

// Inline worker: the pierre worker's module graph is embedded as a blob, so
// the worker never depends on runtime chunk-URL resolution (rspack's worker
// compilation emitted a broken chunk map for the split variant).
export function workerFactory(): Worker {
  return new PierreDiffWorker();
}
