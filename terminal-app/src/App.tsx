import { useCallback, useEffect, useState } from 'react';
import { TerminalPane, type RendererKind } from './TerminalPane';

type ConnState = 'connecting' | 'open' | 'closed';

export function App() {
  const [renderer, setRenderer] = useState<RendererKind>('auto');
  const [conn, setConn] = useState<ConnState>('connecting');
  const [grid, setGrid] = useState<{ cols: number; rows: number } | null>(null);
  // Session key: bump to tear down and recreate the terminal (shell restart
  // and renderer switches both go through it — the Terminal API has no
  // runtime renderer swap, and a full recreate is the honest path).
  const [session, setSession] = useState(0);

  const onConn = useCallback((s: ConnState) => setConn(s), []);
  const onGrid = useCallback((g: { cols: number; rows: number }) => setGrid(g), []);

  // Keep the tab title honest about connection state.
  useEffect(() => {
    document.title = conn === 'open' ? 'terminal-app' : `terminal-app (${conn})`;
  }, [conn]);

  const restart = () => setSession((n) => n + 1);

  return (
    <div className="app">
      <div className="toolbar">
        <h1>
          terminal<b>-app</b>
        </h1>
        <div className="seg" role="group" aria-label="Renderer">
          {(['auto', 'webgl', 'canvas'] as const).map((r) => (
            <button key={r} className={r === renderer ? 'on' : ''} onClick={() => setRenderer(r)}>
              {r}
            </button>
          ))}
        </div>
        <button onClick={restart} title="Restart the shell session">
          ⟳ new shell
        </button>
        <div className="spacer" />
        <span className="status">
          {grid ? (
            <>
              <b>
                {grid.cols}×{grid.rows}
              </b>{' '}
              ·{' '}
            </>
          ) : null}
          <span className={`dot ${conn === 'open' ? 'ok' : conn === 'closed' ? 'err' : ''}`} />
          {conn}
        </span>
      </div>
      <TerminalPane
        key={session}
        renderer={renderer}
        onConn={onConn}
        onGrid={onGrid}
      />
    </div>
  );
}
