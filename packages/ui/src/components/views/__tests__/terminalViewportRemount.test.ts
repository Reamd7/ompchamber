/**
 * Regression guard for slow terminal opening on Linux.
 *
 * `TerminalViewport` is keyed by `terminalViewportKey`. That key used to include
 * the PTY session id, which is null until `createSession` resolves. Historically,
 * the viewport had to mount first to report its size before session creation, so
 * every terminal open built a Ghostty terminal (WASM VT + 2D canvas renderer +
 * font atlas), threw it away when the session id arrived, and built a second one.
 * The same churn repeated on reconnect and on every incidental session-id change,
 * and the repeated WASM terminal allocate/free cycles are the suspected source of
 * the reported crashes.
 *
 * Viewport identity must therefore be directory + tab only. Session changes are
 * handled by the chunk replay path, which resets the existing terminal in place.
 * New sessions start concurrently with a container-derived size (or 80x24) and
 * resize after their viewport fits.
 */
import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const terminalViewSource = readFileSync(join(__dirname, '..', 'TerminalView.tsx'), 'utf-8');
const terminalViewportSource = readFileSync(
    join(__dirname, '..', '..', 'terminal', 'TerminalViewport.tsx'),
    'utf-8',
);

const viewportKeyDeclaration = terminalViewSource
    .split('\n')
    .find((line) => line.includes('const terminalViewportKey =')) ?? '';

describe('terminal viewport remount guard', () => {
    test('viewport identity excludes the PTY session id', () => {
        expect(viewportKeyDeclaration).toContain('effectiveDirectory');
        expect(viewportKeyDeclaration).toContain('activeTabId');
        expect(viewportKeyDeclaration).not.toContain('terminalSessionId');
    });

    test('replay discontinuities reset the terminal in place instead of remounting it', () => {
        const start = terminalViewportSource.indexOf('const recreateRenderer = React.useCallback(');
        expect(start).toBeGreaterThan(-1);
        const body = terminalViewportSource.slice(start, terminalViewportSource.indexOf('}, []);', start));
        expect(body).toContain('terminal.reset()');
        // The generation bump stays only as the fallback when no terminal exists yet.
        expect(body.indexOf('if (!terminal)')).toBeLessThan(body.indexOf('terminal.reset()'));
    });

    test('scrollback is read from the buffer slice, not from the tab', () => {
        expect(terminalViewSource).toContain('getBuffer(');
        expect(terminalViewSource).not.toContain('activeTab?.bufferChunks');
    });

    test('starts the PTY before Ghostty reports its first viewport size', () => {
        expect(terminalViewSource).toContain('const FALLBACK_TERMINAL_SIZE = { cols: 80, rows: 24 } as const;');
        expect(terminalViewSource).toContain('const initialSize = lastViewportSizeRef.current ?? FALLBACK_TERMINAL_SIZE;');
        expect(terminalViewSource).not.toContain('if (!size && isTerminalVisibleRef.current)');
        expect(terminalViewSource).toContain('cols: initialSize.cols');
        expect(terminalViewSource).toContain('rows: initialSize.rows');
        expect(terminalViewSource).toContain('void terminal.resize({ sessionId: session.sessionId, ...viewportSize })');
        expect(terminalViewSource).toContain('if (!isTerminalVisible) {');
        expect(terminalViewSource).not.toContain('isTerminalVisibleRef');
    });

    test('deduplicates create attempts while the viewport layout settles', () => {
        expect(terminalViewSource).toContain('pendingTerminalCreatesRef.current.has(createKey)');
        expect(terminalViewSource).toContain('pendingTerminalCreatesRef.current.delete(createKey)');
    });

    test('lets the session-ID effect own stream startup after creating a tab', () => {
        const createStart = terminalViewSource.indexOf('if (!terminalId) {');
        const createEnd = terminalViewSource.indexOf('if (!terminalId || cancelled) return;', createStart);
        expect(createStart).toBeGreaterThan(-1);
        expect(createEnd).toBeGreaterThan(createStart);
        const createBlock = terminalViewSource.slice(createStart, createEnd);

        expect(createBlock).toContain('setTabSessionId(directory, tabId, session.sessionId);');
        expect(createBlock).toContain('Let that next');
        expect(createBlock).not.toContain('startStream(');
    });

    test('clears a current tab from connecting when a strict-mode create rejects', () => {
        const createStart = terminalViewSource.indexOf('if (!terminalId) {');
        const catchStart = terminalViewSource.indexOf('} catch (error) {', createStart);
        const catchEnd = terminalViewSource.indexOf('} finally {', catchStart);
        expect(catchStart).toBeGreaterThan(createStart);
        expect(catchEnd).toBeGreaterThan(catchStart);
        const catchBlock = terminalViewSource.slice(catchStart, catchEnd);

        expect(catchBlock).toContain('owningTab.terminalSessionId');
        expect(catchBlock).toContain('activeTabIdRef.current !== tabId');
        expect(catchBlock).toContain('setConnecting(directory, tabId, false);');
        expect(catchBlock).not.toContain('if (!cancelled)');
    });

    test('derives the initial PTY size before Ghostty mounts', () => {
        expect(terminalViewportSource).toContain('const getProvisionalTerminalSize');
        expect(terminalViewportSource).toContain('React.useLayoutEffect(() => {');
        expect(terminalViewportSource).toContain('resizeRef.current(size.cols, size.rows)');
        expect(terminalViewportSource).toContain('...(provisionalSizeRef.current ?? {})');
    });
});

describe('terminal viewport zoom contract', () => {
    test('reported effective width is natural capacity divided by zoom', () => {
        // Spec: effective = a / zoom. Zooming OUT shrinks cells so MORE
        // columns fit — the pane stays full and >=80-column TUIs keep
        // working on small panels; zooming in shows fewer, bigger cells.
        expect(terminalViewportSource).toContain('const natural = {');
        expect(terminalViewportSource).toContain('Math.floor(natural.cols / zoom)');
        expect(terminalViewportSource).toContain('Math.floor(natural.rows / zoom)');
    });

    test('display scale is mode-independent: min(zoom, fit-both-axes)', () => {
        // One rule for implicit and forced alike: at the device's own
        // negotiated size grid x zoom fills the container exactly (fit = 1);
        // a narrower sibling grid letterboxes (fit stays 1); a wider claimed
        // grid shrinks to fit.
        const scaleStart = terminalViewportSource.indexOf('const applyViewScale = React.useCallback(');
        const scaleBody = terminalViewportSource.slice(scaleStart, scaleStart + 2200);
        expect(scaleBody).toContain('Math.min(viewZoomRef.current, container.clientWidth / naturalWidth, heightRatio)');
        expect(scaleBody).not.toContain('drivenRef');
    });

    test('zoom changes re-run fit so the renegotiated report reaches the server', () => {
        const setZoomStart = terminalViewportSource.indexOf('const setViewZoom = React.useCallback(');
        const setZoomBody = terminalViewportSource.slice(setZoomStart, setZoomStart + 700);
        expect(setZoomBody).toContain('fit();');
    });

    test('capacity reports survive the first negotiation round', () => {
        // The rendered grid stays pinned to the negotiated size; the report
        // must track the local EFFECTIVE capacity so resizes renegotiate.
        const fitStart = terminalViewportSource.indexOf('const fit = React.useCallback(');
        const fitBody = terminalViewportSource.slice(fitStart, terminalViewportSource.indexOf('}, [applyViewScale]);', fitStart));
        expect(fitBody).toContain('lastSizeRef.current = effective;');
        expect(fitBody).toContain('resizeRef.current(effective.cols, effective.rows);');
    });

    test('view zoom is clamped to (0.25, 4]', () => {
        expect(terminalViewportSource).toContain('Math.max(0.25, Math.min(4, next))');
    });
});
