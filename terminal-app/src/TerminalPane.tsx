import { useEffect, useRef, useState } from 'react';
import { init, Terminal, FitAddon, CanvasRenderer, WebGLRenderer } from 'ghostty-web';
import { createTransport } from './transport';
import { GridTransport } from './grid/GridTransport';
import type { IRenderable } from 'ghostty-web';

export type RendererKind = 'auto' | 'webgl' | 'canvas';
export type FeedKind = 'pty' | 'grid';
type ConnState = 'connecting' | 'open' | 'closed';

interface Props {
  renderer: RendererKind;
  feed: FeedKind;
  onConn: (s: ConnState) => void;
  onGrid: (g: { cols: number; rows: number }) => void;
}

/**
 * One terminal session. Two feeds share the same renderers:
 *  - 'pty': ghostty-web Terminal, VT parsing in the browser (wasm)
 *  - 'grid': server-side parsing — rows arrive parsed over WS and a
 *    GridTerminal adapter presents them as an IRenderable, so the
 *    Canvas/WebGL renderers run unchanged.
 * Keyed by the App on restart; renderer/feed changes recreate the
 * session (no runtime swap — an honest teardown beats half-state).
 */
export function TerminalPane({ renderer, feed, onConn, onGrid }: Props) {
  const hostRef = useRef<HTMLDivElement>(null);
  const [fatal, setFatal] = useState<string | null>(null);

  useEffect(() => {
    let disposed = false;
    let ro: ResizeObserver | null = null;
    let framePending = false;
    let cleanupFns: Array<() => void> = [];

    const start = async () => {
      onConn('connecting');
      try {
        if (feed === 'pty') await init();
      } catch (e) {
        setFatal(`wasm init failed: ${String(e)}`);
        return;
      }
      if (disposed) return;
      const host = hostRef.current;
      if (!host) return;

      const fontOptions = {
        fontSize: 15,
        fontFamily: '"Cascadia Mono", Consolas, "Courier New", monospace',
        theme: { background: '#1e1e1e', foreground: '#d4d4d4' },
      };

      if (feed === 'pty') {
        // ---- Browser-side parsing (wasm) ----
        const term = new Terminal({
          renderer,
          cursorBlink: true,
          scrollback: 10000,
          ...fontOptions,
        });
        const fit = new FitAddon();
        term.loadAddon(fit);
        term.open(host);
        fit.fit();
        onGrid({ cols: term.cols, rows: term.rows });

        const transport = createTransport();
        term.onResize((size) => {
          onGrid({ cols: size.cols, rows: size.rows });
          transport.resize(size.cols, size.rows);
        });
        term.onData((data) => transport.write(data));
        transport.onData((chunk) => term.write(chunk));
        transport.onConnState(onConn);
        await transport.connect({ cols: term.cols, rows: term.rows });

        ro = new ResizeObserver(() => {
          try {
            fit.fit();
          } catch {
            /* metrics not settled yet; next observation retries */
          }
        });
        ro.observe(host);
        cleanupFns.push(() => {
          transport.close();
          term.dispose();
        });
        return;
      }

      // ---- Server-side parsing (grid push) ----
      // The renderers run against the GridTerminal adapter directly.
      const grid = new GridTransport();
      let draw: (buffer: IRenderable, force: boolean) => void;
      let currentRenderer: CanvasRenderer | WebGLRenderer;
      if (renderer === 'canvas') {
        currentRenderer = new CanvasRenderer(host.appendChild(document.createElement('canvas')), {
          ...fontOptions,
        });
      } else {
        try {
          currentRenderer = new WebGLRenderer(host.appendChild(document.createElement('canvas')), {
            ...fontOptions,
          });
        } catch (e) {
          if (renderer === 'webgl') throw e;
          console.warn('[term] GL unavailable, grid feed falling back to canvas:', e);
          currentRenderer = new CanvasRenderer(host.appendChild(document.createElement('canvas')), {
            ...fontOptions,
          });
        }
      }
      const rend = currentRenderer;

      const fitNow = () => {
        const m = rend.getMetrics();
        const cols = Math.max(2, Math.floor((host.clientWidth - 4) / m.width));
        const rows = Math.max(2, Math.floor((host.clientHeight - 4) / m.height));
        const dims = grid.model.getDimensions();
        if (dims.cols !== cols || dims.rows !== rows) {
          // resize the renderer canvas first, then ask the server
          rend.resize(cols, rows);
          grid.resize(cols, rows);
          onGrid({ cols, rows });
          scheduleFrame(true);
        }
      };

      const scheduleFrame = (force = false) => {
        if (framePending && !force) return;
        framePending = true;
        requestAnimationFrame(() => {
          framePending = false;
          if (disposed) return;
          rend.render(grid.model, force);
        });
      };

      grid.onFrame = () => scheduleFrame(false);
      grid.onConnState(onConn);

      // Keyboard: the renderers' Terminal integration owns the hidden
      // textarea; in grid mode we mount one ourselves.
      const ta = host.appendChild(document.createElement('textarea'));
      ta.style.cssText =
        'position:absolute;opacity:0;left:-9999px;width:1px;height:1px;';
      ta.addEventListener('input', () => {
        if (ta.value) {
          grid.write(ta.value);
          ta.value = '';
        }
      });
      ta.addEventListener('keydown', (ev) => {
        const k = ev.key;
        let seq: string | null = null;
        if (k === 'Enter') seq = '\r';
        else if (k === 'Backspace') seq = '\x7f';
        else if (k === 'Tab') seq = '\t';
        else if (k === 'Escape') seq = '\x1b';
        else if (k === 'ArrowUp') seq = '\x1b[A';
        else if (k === 'ArrowDown') seq = '\x1b[B';
        else if (k === 'ArrowRight') seq = '\x1b[C';
        else if (k === 'ArrowLeft') seq = '\x1b[D';
        else if (k.length > 1 && (ev.ctrlKey || ev.metaKey)) {
          // Ctrl+letter etc.
          const c = k.toLowerCase().charCodeAt(0);
          if (c >= 97 && c <= 122) seq = String.fromCharCode(c - 96);
        }
        if (seq !== null) {
          ev.preventDefault();
          grid.write(seq);
        }
      });
      host.addEventListener('mousedown', () => ta.focus());
      ta.focus();

      // Fit first so the server spawns the PTY at the visible size;
      // the URL-carried grid must match the renderer's fit.
      const m0 = rend.getMetrics();
      const c0 = Math.max(2, Math.floor((host.clientWidth - 4) / m0.width));
      const r0 = Math.max(2, Math.floor((host.clientHeight - 4) / m0.height));
      await grid.connect({ cols: c0, rows: r0 });
      requestAnimationFrame(() => {
        if (!disposed) fitNow();
      });
      // Server sends the initial full frame; refit once dimensions are known.
      setTimeout(fitNow, 300);

      ro = new ResizeObserver(fitNow);
      ro.observe(host);

      draw = (buffer, force) => rend.render(buffer, force);
      void draw;

      cleanupFns.push(() => {
        grid.close();
        rend.dispose?.();
        ta.remove();
      });
    };

    start().catch((e) => setFatal(String(e)));

    return () => {
      disposed = true;
      ro?.disconnect();
      for (const fn of cleanupFns) fn();
      cleanupFns = [];
    };
  }, [renderer, feed, onConn, onGrid]);

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
