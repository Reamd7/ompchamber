/**
 * @typedef {import('ws').WebSocketServer} WebSocketServerT
 */
/**
 * @typedef {object} AttachGridServerOptions
 * @property {string} [path] Upgrade path to claim (default '/gridws').
 * @property {WebSocketServerT} [wss] Reuse an existing no-server
 *   WebSocketServer; upgrades for other paths are ignored instead of
 *   destroyed so the host can multiplex endpoints.
 */
/**
 * @typedef {object} AttachedGridServer
 * @property {WebSocketServerT} wss
 * @property {() => string} issueToken Mint a one-time connection token
 *   (expose via your own /api). While no tokens have been minted the
 *   endpoint accepts any non-empty token — hosts with their own auth
 *   never call issueToken.
 */
/**
 * @param {import('node:http').Server} httpServer
 * @param {AttachGridServerOptions} [options]
 * @returns {AttachedGridServer}
 */
export function attachGridServer(httpServer: import("node:http").Server, { path, wss }?: AttachGridServerOptions): AttachedGridServer;
/**
 * Standalone entry: an http server with the grid endpoint attached.
 * @param {{port?: number, host?: string, path?: string}} [options]
 * @returns {Promise<{httpServer: import('node:http').Server, grid: AttachedGridServer}>}
 */
export function createGridServer({ port, host, path }?: {
    port?: number;
    host?: string;
    path?: string;
}): Promise<{
    httpServer: import("node:http").Server;
    grid: AttachedGridServer;
}>;
export type WebSocketServerT = import("ws").WebSocketServer;
export type AttachGridServerOptions = {
    /**
     * Upgrade path to claim (default '/gridws').
     */
    path?: string | undefined;
    /**
     * Reuse an existing no-server
     * WebSocketServer; upgrades for other paths are ignored instead of
     * destroyed so the host can multiplex endpoints.
     */
    wss?: WebSocketServer | undefined;
};
export type AttachedGridServer = {
    wss: WebSocketServerT;
    /**
     * Mint a one-time connection token
     * (expose via your own /api). While no tokens have been minted the
     * endpoint accepts any non-empty token — hosts with their own auth
     * never call issueToken.
     */
    issueToken: () => string;
};
import { WebSocketServer } from 'ws';
