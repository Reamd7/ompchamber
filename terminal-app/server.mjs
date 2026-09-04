#!/usr/bin/env node
/**
 * terminal-app PTY server — minimal backend for the standalone app.
 *
 * Shape follows references/ghostty-web/demo/bin/demo.js (token auth +
 * WS PTY + resize control) but trimmed to what this app needs:
 *   GET  /api/token   -> one-time token
 *   WS   /ws?token=.. -> PTY stream (raw strings both ways)
 *                        client may send {"type":"resize","cols":C,"rows":R}
 */

import crypto from 'node:crypto';
import http from 'node:http';
import { WebSocketServer } from 'ws';
import pty from '@lydell/node-pty';

const PORT = Number(process.env.PORT ?? 8081);

const server = http.createServer((req, res) => {
  if (req.url === '/api/token') {
    const token = crypto.randomBytes(16).toString('hex');
    pendingTokens.add(token);
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ token }));
    return;
  }
  res.writeHead(404);
  res.end('not found');
});

const pendingTokens = new Set();
const wss = new WebSocketServer({ noServer: true });

server.on('upgrade', (req, socket, head) => {
  const url = new URL(req.url, 'http://localhost');
  if (url.pathname !== '/ws') {
    socket.destroy();
    return;
  }
  const token = url.searchParams.get('token') ?? '';
  if (!pendingTokens.delete(token)) {
    socket.write('HTTP/1.1 401 Unauthorized\r\n\r\n');
    socket.destroy();
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => {
    const cols = Math.max(2, Math.min(500, Number(url.searchParams.get('cols')) || 80));
    const rows = Math.max(2, Math.min(300, Number(url.searchParams.get('rows')) || 24));

    const shell = process.env.ComSpec || 'cmd.exe';
    const proc = pty.spawn(shell, [], {
      name: 'xterm-256color',
      cols,
      rows,
      cwd: process.env.USERPROFILE || process.cwd(),
      env: { ...process.env, TERM: 'xterm-256color' },
    });

    proc.onData((data) => {
      if (ws.readyState === ws.OPEN) ws.send(data);
    });
    proc.onExit(() => {
      if (ws.readyState === ws.OPEN) ws.close();
    });

    ws.on('message', (raw) => {
      const text = raw.toString();
      if (text.startsWith('{') && text.includes('"type":"resize"')) {
        try {
          const msg = JSON.parse(text);
          if (msg.type === 'resize' && Number.isFinite(msg.cols) && Number.isFinite(msg.rows)) {
            proc.resize(
              Math.max(2, Math.min(500, msg.cols | 0)),
              Math.max(2, Math.min(300, msg.rows | 0))
            );
            return;
          }
        } catch {
          /* fall through: treat as input */
        }
      }
      proc.write(text);
    });
    ws.on('close', () => proc.kill());
  });
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`[terminal-app] PTY server on http://127.0.0.1:${PORT}`);
});
