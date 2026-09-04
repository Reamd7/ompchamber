/**
 * attachGridServer — WS transport over GridPtySession.
 *
 *   const httpServer = http.createServer(...);
 *   const grid = attachGridServer(httpServer, { path: '/gridws' });
 *   grid.issueToken() -> string        // one-time tokens for /grid-token
 *
 * Client protocol (same frames GridCore produces):
 *   connect: GET {path}?token=..&cols=..&rows=..
 *   input:   raw text, or {"type":"resize","cols":C,"rows":R}
 *   output:  full/rows/cursor JSON frames
 */

import crypto from 'node:crypto';
import http from 'node:http';
import { WebSocketServer } from 'ws';
import { GridPtySession } from './pty-session.mjs';

export function attachGridServer(httpServer, { path = '/gridws', wss } = {}) {
  const tokens = new Set();
  const server = wss || new WebSocketServer({ noServer: true });

  httpServer.on('upgrade', (req, socket, head) => {
    const url = new URL(req.url, 'http://localhost');
    if (url.pathname !== path) {
      if (wss) return; // someone else's path: ignore, don't destroy
      socket.destroy();
      return;
    }
    // Token policy is the host's business; default = one-time tokens
    // from issueToken(). Hosts with their own auth may pass tokens=null.
    const token = url.searchParams.get('token');
    if (tokens.size > 0 || token === null) {
      if (!tokens.delete(token ?? '')) {
        socket.write('HTTP/1.1 401 Unauthorized\r\n\r\n');
        socket.destroy();
        return;
      }
    }
    server.handleUpgrade(req, socket, head, (ws) => {
      const cols = Number(url.searchParams.get('cols')) || 80;
      const rows = Number(url.searchParams.get('rows')) || 24;
      const session = new GridPtySession({ cols, rows });
      session.onFrame = (frame) => {
        if (ws.readyState === ws.OPEN) ws.send(JSON.stringify(frame));
      };
      session.onExit = () => ws.readyState === ws.OPEN && ws.close();

      ws.on('message', (raw) => {
        const text = raw.toString();
        if (text.startsWith('{')) {
          try {
            const msg = JSON.parse(text);
            if (msg.type === 'resize' && msg.cols && msg.rows) {
              session.resize(msg.cols, msg.rows);
              return;
            }
          } catch {
            /* fall through as input */
          }
        }
        session.write(text);
      });
      ws.on('close', () => session.dispose());
    });
  });

  return {
    wss: server,
    /** Mint a one-time connection token (expose via your own /api). */
    issueToken() {
      const t = crypto.randomBytes(16).toString('hex');
      tokens.add(t);
      return t;
    },
  };
}

/** Standalone entry: an http server with the grid endpoint attached. */
export function createGridServer({ port = 8082, host = '127.0.0.1', path } = {}) {
  const httpServer = http.createServer((req, res) => {
    res.writeHead(404);
    res.end();
  });
  const grid = attachGridServer(httpServer, { path });
  return new Promise((resolve) => {
    httpServer.listen(port, host, () => resolve({ httpServer, grid }));
  });
}
