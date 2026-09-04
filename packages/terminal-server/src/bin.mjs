#!/usr/bin/env node
/**
 * Standalone grid server bin (terminal-app dev entry).
 *
 * Token policy: none minted here — the server accepts any non-empty
 * token (terminal-app's client carries the pty server's token; this is
 * an experiment-scope relaxation, documented in server.mjs).
 */
import { createGridServer } from './server.mjs';

const PORT = Number(process.env.GRID_PORT ?? 8082);
await createGridServer({ port: PORT });
console.log(`[terminal-server] grid server on http://127.0.0.1:${PORT}`);
