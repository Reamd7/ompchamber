export function workerFactory(): Worker {
  return new Worker(new URL('./pierre-diff.worker.ts', import.meta.url), { type: 'module' });
}
