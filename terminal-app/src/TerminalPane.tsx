import { useEffect, useRef, useState } from 'react';
import { init, Terminal, FitAddon } from 'ghostty-web';
import { createTransport } from './transport';

export type RendererKind = 'auto' | 'webgl' | 'canvas';
type ConnState = 'connecting' | 'open' | 'closed';

interface Props {
  renderer: RendererKind;
  onConn: (s: ConnState) => void;
  onGrid: (g: { cols: number; rows: number }) => void;
}

/**
 * One terminal session: ghostty-web Terminal + FitAddon in a sized
 * container, wired to a PTY through an environment-selected transport
 * (IPC in Electron, WebSocket in the browser). Keyed by the App on
 * shell restart; renderer changes also recreate the session (the
 * Terminal API has no runtime renderer swap — an honest teardown beats
 * a half-swapped canvas).
 */
export function TerminalPane({ renderer, onConn, onGrid }: Props) {
  const hostRef = useRef<HTMLDivElement>(null);
  const [fatal, setFatal] = useState<string | null>(null);

  useEffect(() => {
    let disposed = false;
    let term: Terminal | null = null;
    let fit: FitAddon | null = null;
    let ro: ResizeObserver | null = null;
    const transport = createTransport();

    const start = async () => {
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
      onGrid({ cols: term.cols, rows: term.rows });

      term.onResize((size) => {
        onGrid({ cols: size.cols, rows: size.rows });
        transport.resize(size.cols, size.rows);
      });
      term.onData((data) => transport.write(data));
      transport.onData((chunk) => term?.write(chunk));
      transport.onConnState(onConn);

      await transport.connect({ cols: term.cols, rows: term.rows });

      // Refit on container size changes (window resizes, devtools open...).
      ro = new ResizeObserver(() => {
        try {
          fit?.fit();
        } catch {
          /* fit before metrics settle throws; next observation retries */
        }
      });
      ro.observe(host);
    };

    start();

    return () => {
      disposed = true;
      ro?.disconnect();
      transport.close();
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
