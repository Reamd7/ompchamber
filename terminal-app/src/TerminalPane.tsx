import { useEffect, useRef, useState } from 'react';
import { init, Terminal, FitAddon } from 'ghostty-web';

export type RendererKind = 'auto' | 'webgl' | 'canvas';
type ConnState = 'connecting' | 'open' | 'closed';

interface Props {
  renderer: RendererKind;
  onConn: (s: ConnState) => void;
  onGrid: (g: { cols: number; rows: number }) => void;
}

/**
 * One terminal session: ghostty-web Terminal + FitAddon in a sized
 * container, wired to the PTY server over a WebSocket. The component is
 * keyed by the App on shell restart; renderer changes also recreate the
 * session (the Terminal API has no runtime renderer swap — an honest
 * teardown beats a half-swapped canvas).
 */
export function TerminalPane({ renderer, onConn, onGrid }: Props) {
  const hostRef = useRef<HTMLDivElement>(null);
  const [fatal, setFatal] = useState<string | null>(null);

  useEffect(() => {
    let disposed = false;
    let term: Terminal | null = null;
    let fit: FitAddon | null = null;
    let ws: WebSocket | null = null;
    let ro: ResizeObserver | null = null;
    let reconnectTimer: number | undefined;
    // Pending resize sent on open (grid is known after fit).
    let lastGrid: { cols: number; rows: number } | null = null;

    const connect = async () => {
      onConn('connecting');
      try {
        await init();
      } catch (e) {
        setFatal(`wasm init failed: ${String(e)}`);
        return;
      }
      if (disposed) return;

      const host = hostRef.current;
      if (!host) return;

      term = new Terminal({
        renderer,
        fontSize: 15,
        fontFamily: '"Cascadia Mono", Consolas, "Courier New", monospace',
        cursorBlink: true,
        scrollback: 10000,
        theme: {
          background: '#1e1e1e',
          foreground: '#d4d4d4',
        },
      });
      fit = new FitAddon();
      term.loadAddon(fit);
      term.open(host);
      fit.fit();
      lastGrid = { cols: term.cols, rows: term.rows };
      onGrid(lastGrid);

      term.onResize((size) => {
        lastGrid = { cols: size.cols, rows: size.rows };
        onGrid(lastGrid);
        if (ws?.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ type: 'resize', cols: size.cols, rows: size.rows }));
        }
      });
      term.onData((data) => {
        if (ws?.readyState === WebSocket.OPEN) ws.send(data);
      });

      // Refit on container size changes (window resizes, devtools open...).
      ro = new ResizeObserver(() => {
        try {
          fit?.fit();
        } catch {
          /* fit before metrics settle throws; next observation retries */
        }
      });
      ro.observe(host);

      const openSocket = async () => {
        let token = '';
        try {
          const res = await fetch('/api/token', { cache: 'no-store' });
          token = (await res.json()).token;
        } catch {
          onConn('closed');
          reconnectTimer = window.setTimeout(openSocket, 2000);
          return;
        }
        if (disposed) return;
        const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
        ws = new WebSocket(
          `${proto}//${location.host}/ws?token=${encodeURIComponent(token)}&cols=${term!.cols}&rows=${term!.rows}`
        );
        ws.onopen = () => onConn('open');
        ws.onmessage = (ev) => term?.write(ev.data);
        ws.onclose = () => {
          if (disposed) return;
          onConn('closed');
          reconnectTimer = window.setTimeout(openSocket, 2000);
        };
        ws.onerror = () => ws?.close();
      };
      openSocket();
    };

    connect();

    return () => {
      disposed = true;
      if (reconnectTimer) clearTimeout(reconnectTimer);
      ro?.disconnect();
      ws?.close();
      term?.dispose();
    };
  }, [renderer, onConn, onGrid]);

  return (
    <div className="term-host">
      {fatal ? (
        <div style={{ padding: 24, color: '#e05252' }}>{fatal}</div>
      ) : (
        <div className="terminal" ref={hostRef} />
      )}
    </div>
  );
}
