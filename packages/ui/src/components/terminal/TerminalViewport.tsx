import React from 'react';
import type { FitAddon, Ghostty, Terminal as GhosttyTerminal } from 'ghostty-web';

import { cn } from '@/lib/utils';
import type { TerminalTheme } from '@/lib/terminalTheme';
import { TERMINAL_TYPOGRAPHY, getGhosttyTerminalOptions } from '@/lib/terminalTheme';
import {
  getGhosttySafeResetSequence,
  rewriteGhosttyDefaultBackgroundResets,
} from '@/lib/terminalOutput';
import {
  getTerminalCellFromPoint,
  getTerminalWordRange,
  type TerminalCellPosition,
} from '@/lib/terminalTouchSelection';
import type { TerminalChunk } from '@/stores/useTerminalStore';

// ghostty-web (638 KB raw of JS + the WASM VT) loads on demand: TerminalView
// stays eagerly importable for the bottom dock without pulling the emulator
// into the startup graph before a terminal is actually mounted.
type GhosttyModule = typeof import('ghostty-web');
type GhosttyRuntime = { module: GhosttyModule; ghostty: Ghostty };
let ghosttyRuntimePromise: Promise<GhosttyRuntime> | null = null;
const loadGhostty = (): Promise<GhosttyRuntime> =>
  ghosttyRuntimePromise ??= import('ghostty-web').then(async (module) => ({
    module,
    ghostty: await module.Ghostty.load(),
  }));

// The web entry defers its ~2 MB Nerd Font download until a terminal actually
// mounts (see the `__ompchamberEnsureNerdFonts` hook in index.html). Wait for
// it with a short bound so a cached font is in place before the glyph atlas is
// built, while a cold CDN fetch never blocks the terminal from opening; the
// runtimes without the hook (VS Code, mobile) resolve immediately.
const NERD_FONT_WAIT_MS = 2000;
const ensureNerdFonts = (): Promise<void> => {
  if (typeof window === 'undefined') return Promise.resolve();
  const loader = (window as typeof window & { __ompchamberEnsureNerdFonts?: () => Promise<void> }).__ompchamberEnsureNerdFonts;
  if (typeof loader !== 'function') return Promise.resolve();
  return Promise.race([
    Promise.resolve(loader()).catch(() => undefined),
    new Promise<void>((resolve) => setTimeout(resolve, NERD_FONT_WAIT_MS)),
  ]).then(() => undefined);
};

type TerminalSize = { cols: number; rows: number };

const getProvisionalTerminalSize = (
  container: HTMLDivElement,
  fontFamily: string,
  fontSize: number,
): TerminalSize | null => {
  if (typeof window === 'undefined' || typeof document === 'undefined') return null;

  const context = document.createElement('canvas').getContext('2d');
  if (!context || container.clientWidth < 24 || container.clientHeight < 24) return null;

  // Mirror the renderer's measureFont exactly (weight in the font string,
  // lineHeight scaling over ascent+descent). A mismatch here spawns the PTY
  // with more rows than fit; the post-mount fit then shrinks the grid and
  // full-screen TUIs (btop, fresh) redraw misaligned — their top row ends up
  // covered by the pane header.
  context.font = `${TERMINAL_TYPOGRAPHY.fontWeight} ${fontSize}px ${fontFamily}`;
  const metrics = context.measureText('M');
  const cellWidth = Math.ceil(metrics.width);
  const naturalHeight = Math.ceil(
    (metrics.actualBoundingBoxAscent || fontSize * 0.8) +
    (metrics.actualBoundingBoxDescent || fontSize * 0.2),
  );
  const cellHeight = Math.max(1, Math.ceil(naturalHeight * TERMINAL_TYPOGRAPHY.lineHeight));
  if (cellWidth < 1 || cellHeight < 1) return null;

  const style = window.getComputedStyle(container);
  const horizontalPadding =
    (Number.parseInt(style.paddingLeft, 10) || 0) +
    (Number.parseInt(style.paddingRight, 10) || 0);
  const verticalPadding =
    (Number.parseInt(style.paddingTop, 10) || 0) +
    (Number.parseInt(style.paddingBottom, 10) || 0);

  // Match Ghostty FitAddon's 15px scrollbar reservation and minimum dimensions.
  // No grid floor here: the server floors the create request itself, and the
  // client must report true viewport sizes for driver zoom to work.
  return {
    cols: Math.max(2, Math.floor((container.clientWidth - horizontalPadding - 15) / cellWidth)),
    rows: Math.max(1, Math.floor((container.clientHeight - verticalPadding) / cellHeight)),
  };
};

export type TerminalController = {
  focus: () => void;
  fit: () => void;
  resizeGrid: (cols: number, rows: number) => void;
  zoomIn: () => void;
  zoomOut: () => void;
  zoomReset: () => void;
  getZoom: () => number;
  getSelection: () => { text: string; startLine: number; endLine: number } | null;
  /** Direct write into the Ghostty terminal, bypassing the React chunk
   * pipeline. Used by the streaming fast path where per-chunk React
   * reconciliation and Blink layout invalidation accumulate into multi-GB
   * renderer RSS (PartitionAlloc never returns pages to the OS). */
  writeDirect: (data: string, sequence?: number) => void;
};

type Props = {
  sessionKey: string;
  chunks: TerminalChunk[];
  onInput: (data: string) => void;
  onResize: (cols: number, rows: number) => void;
  theme: TerminalTheme;
  fontFamily: string;
  fontSize: number;
  className?: string;
  enableTouchScroll?: boolean;
  /** View-zoom change notifier (pure visual scale; 1 = fit). */
  onZoomChange?: (zoom: number) => void;
  autoFocus?: boolean;
  isVisible?: boolean;
  /**
   * Renderer-consumption signal: called with the server sequence of the last
   * chunk the Ghostty renderer has fully consumed. Backpressure for the
   * terminal stream acks on this, not on ws receipt — otherwise a renderer
   * that falls behind lets the browser buffer the flood unboundedly.
   */
  onConsumed?: (sequence: number) => void;
};
const TerminalViewport = React.forwardRef<TerminalController, Props>(({
  sessionKey, chunks, onInput, onResize, theme, fontFamily, fontSize, className,
  enableTouchScroll = false, onZoomChange, autoFocus = true, isVisible = true, onConsumed,
}, ref) => {
  const containerRef = React.useRef<HTMLDivElement>(null);
  // Forced-ownership mode flag: when a device has claimed the grid, non-owners
  // auto-fit-scale the grid into their container; in implicit mode everyone
  // renders natively and wider devices letterbox.
  const terminalRef = React.useRef<GhosttyTerminal | null>(null);
  const fitRef = React.useRef<FitAddon | null>(null);
  const inputRef = React.useRef(onInput);
  const resizeRef = React.useRef(onResize);
  const lastSizeRef = React.useRef<TerminalSize | null>(null);
  const provisionalSizeRef = React.useRef<TerminalSize | null>(null);
  const lastChunkRef = React.useRef<number | null>(null);
  const writeQueueRef = React.useRef('');
  const outputRewriteCarryRef = React.useRef('');
  const safeResetRef = React.useRef(getGhosttySafeResetSequence(theme.background));
  const writingRef = React.useRef(false);
  // Incremented whenever the replay stream restarts, so a write completing from
  const queuedSequenceRef = React.useRef<number | null>(null);
  const onConsumedRef = React.useRef(onConsumed);
  onConsumedRef.current = onConsumed;
  // before the restart cannot clear the in-flight flag of a newer write.
  const writeEpochRef = React.useRef(0);
  const visibleRef = React.useRef(isVisible);
  const rendererReadyRef = React.useRef(false);
  const [ready, setReady] = React.useState(0);
  const [rendererGeneration, setRendererGeneration] = React.useState(0);
  inputRef.current = onInput;
  resizeRef.current = onResize;
  visibleRef.current = isVisible;
  safeResetRef.current = getGhosttySafeResetSequence(theme.background);

  React.useLayoutEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const size = getProvisionalTerminalSize(container, fontFamily, fontSize);
    provisionalSizeRef.current = size;
    if (size) resizeRef.current(size.cols, size.rows);
  }, [fontFamily, fontSize]);

  // Server-negotiated grid from multi-device min-size arbitration. When set,
  // fit() clamps to it so a large viewport doesn't fight the server's resize.
  const negotiatedGridRef = React.useRef<{ cols: number; rows: number } | null>(null);
  // Driver-follow scaling: when the negotiated grid (set by the driver
  // device's viewport) is wider than this container, CSS-scale the canvas
  // down so the full grid stays visible. All gesture math is ratio-based
  // against the canvas rect, so the transform doesn't break cell mapping.
  // View zoom — the display scale, clamped to (0.25, 4]. It participates in
  // the reported effective width as capacity / zoom: zooming out reports
  // MORE columns (smaller cells, pane stays full), zooming in reports
  // fewer, bigger cells.
  const viewZoomRef = React.useRef(1);
  const zoomApiRef = React.useRef<{ setViewZoom: (next: number) => void } | null>(null);
  const [viewZoom, setViewZoomState] = React.useState(1);
  const setViewZoom = React.useCallback((next: number) => {
    const clamped = Math.max(0.25, Math.min(4, next));
    viewZoomRef.current = clamped;
    setViewZoomState(clamped);
    applyViewScale();
    fit();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  zoomApiRef.current = { setViewZoom };
  const applyViewScale = React.useCallback(() => {
    const container = containerRef.current;
    const canvas = container?.querySelector('canvas');
    if (!container || !canvas) return;
    const naturalWidth = canvas.offsetWidth;
    const naturalHeight = canvas.offsetHeight;
    if (naturalWidth <= 0 || naturalHeight <= 0 || container.clientWidth <= 0) return;
    // One display rule for every mode: the rendered grid is drawn at
    // min(zoom, fit-both-axes). At the device's own negotiated size the grid
    // x zoom fills the container exactly (fit = 1). When another device's
    // grid is narrower, fit stays 1 and the leftover space letterboxes; when
    // it is wider (forced claim by a bigger screen), fit shrinks the whole
    // grid into view.
    const heightRatio = container.clientHeight > 0 ? container.clientHeight / naturalHeight : 1;
    const finalScale = Math.max(0.2, Math.min(viewZoomRef.current, container.clientWidth / naturalWidth, heightRatio));
    if (finalScale > 0.999 && finalScale < 1.001) {
      canvas.style.transform = '';
      canvas.style.transformOrigin = '';
    } else {
      canvas.style.transform = `scale(${finalScale})`;
      canvas.style.transformOrigin = 'top left';
    }
  }, []);



  const fit = React.useCallback(() => {
    const container = containerRef.current;
    const terminal = terminalRef.current;
    if (!container || !terminal || !visibleRef.current) return;
    const bounds = container.getBoundingClientRect();
    if (bounds.width < 24 || bounds.height < 24) return;
    try {
      const metrics = (terminal as unknown as { renderer?: { getMetrics?: () => { width: number; height: number } | undefined } }).renderer?.getMetrics?.();
      if (!metrics || metrics.width <= 0 || metrics.height <= 0) {
        requestAnimationFrame(() => fit());
        return;
      }
      const style = window.getComputedStyle(container);
      const padX = (Number.parseInt(style.paddingLeft, 10) || 0) + (Number.parseInt(style.paddingRight, 10) || 0);
      const padY = (Number.parseInt(style.paddingTop, 10) || 0) + (Number.parseInt(style.paddingBottom, 10) || 0);
      // a = columns the container displays at 100% zoom (FitAddon math incl.
      // the 15px scrollbar reservation). The REPORTED effective width is
      // a / zoom: zooming OUT shrinks the cells, so MORE grid columns fit
      // in the same container — the pane stays completely full and TUIs
      // that need >=80 columns keep working on small panels. Zooming in
      // shows fewer, bigger cells.
      const zoom = viewZoomRef.current;
      const natural = {
        cols: Math.max(2, Math.floor((container.clientWidth - padX - 15) / metrics.width)),
        rows: Math.max(1, Math.floor((container.clientHeight - padY) / metrics.height)),
      };
      const effective = {
        cols: Math.max(2, Math.floor(natural.cols / zoom)),
        rows: Math.max(1, Math.floor(natural.rows / zoom)),
      };
      const negotiated = negotiatedGridRef.current;
      // Render the negotiated grid when the server set one; implicit-mode
      // letterboxing comes free — a smaller grid in a bigger container.
      const render = negotiated ?? effective;
      if (terminal.cols !== render.cols || terminal.rows !== render.rows) {
        terminal.resize(render.cols, render.rows);
      }
      // Always report the EFFECTIVE size, never the rendered grid — the
      // report is this device's negotiation input, and the rendered grid is
      // the negotiation OUTPUT; feeding it back would pin the grid forever.
      const last = lastSizeRef.current;
      if (!last || last.cols !== effective.cols || last.rows !== effective.rows) {
        lastSizeRef.current = effective;
        resizeRef.current(effective.cols, effective.rows);
      }
      requestAnimationFrame(() => applyViewScale());
    } catch { /* hidden or detached */ }
  }, [applyViewScale]);

  // Zoom divides the cell size in the capacity formula, so every zoom step
  // must renegotiate the grid (more columns out, fewer in) — rescaling the
  // canvas alone would leave the same grid shrunk with margins.
  React.useEffect(() => {
    fit();
  }, [viewZoom, fit]);


  const flush = React.useCallback(() => {
    if (writingRef.current || !writeQueueRef.current || !terminalRef.current) return;
    const terminal = terminalRef.current;
    const pending = writeQueueRef.current;
    writeQueueRef.current = '';
    const rewritten = rewriteGhosttyDefaultBackgroundResets(
      pending,
      outputRewriteCarryRef.current,
      safeResetRef.current,
    );
    outputRewriteCarryRef.current = rewritten.carry;
    if (!rewritten.data) {
      if (writeQueueRef.current) flush();
      return;
    }
    writingRef.current = true;
    const epoch = writeEpochRef.current;
    try {
      terminal.write(rewritten.data, () => {
        if (terminalRef.current !== terminal || writeEpochRef.current !== epoch) return;
        writingRef.current = false;
        // Report renderer consumption when the queue is fully drained: this
        // is the signal the transport acks on, so a slow renderer applies
        // backpressure instead of buffering the flood in the browser.
        const sequence = queuedSequenceRef.current;
        if (sequence != null && !writeQueueRef.current) {
          queuedSequenceRef.current = null;
          onConsumedRef.current?.(sequence);
        }
        if (writeQueueRef.current) flush();
      });
    } catch (error) {
      // The wasm allocator refuses oversized allocations by throwing (rather
      // than corrupting wasm memory). Dropping the chunk is the only safe
      // degradation; the stream continues with the next frame.
      writingRef.current = false;
      console.error('[terminal] write failed, dropping chunk', error);
    }
  }, []);

  /**
   * Replay discontinuities (restart, reconnect, buffer reset) only need the VT
   * state cleared. `Terminal.reset()` frees and rebuilds the WASM terminal while
   * keeping the canvas, renderer and font atlas, so prefer it over remounting the
   * whole terminal; the generation bump remains the fallback before the terminal
   * exists.
   */
  const recreateRenderer = React.useCallback(() => {
    lastChunkRef.current = null;
    writeQueueRef.current = '';
    outputRewriteCarryRef.current = '';
    writingRef.current = false;
    writeEpochRef.current += 1;
    const terminal = terminalRef.current;
    if (!terminal) {
      setRendererGeneration((value) => value + 1);
      return;
    }
    try {
      terminal.reset();
      const safeReset = safeResetRef.current;
      if (safeReset) terminal.write(`${safeReset}\u001b[2J\u001b[H`);
    } catch {
      setRendererGeneration((value) => value + 1);
    }
  }, []);

  React.useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    let disposed = false;
    let terminal: GhosttyTerminal | null = null;
    let observer: ResizeObserver | null = null;
    let canvasObserver: ResizeObserver | null = null;
    let resizeTimeout: ReturnType<typeof setTimeout> | null = null;
    let fitFrame: number | null = null;
    let subscriptions: Array<{ dispose: () => void }> = [];
    const handleFocusIn = () => {
      if (terminal && visibleRef.current) terminal.options.cursorBlink = true;
    };
    const handleFocusOut = (event: FocusEvent) => {
      if (event.relatedTarget instanceof Node && container.contains(event.relatedTarget)) return;
      if (terminal) terminal.options.cursorBlink = false;
    };
    const handleWindowFocus = () => {
      if (terminal && visibleRef.current && container.contains(document.activeElement)) {
        terminal.options.cursorBlink = true;
      }
    };
    const handleWindowBlur = () => {
      if (terminal) terminal.options.cursorBlink = false;
    };

    container.addEventListener('focusin', handleFocusIn);
    container.addEventListener('focusout', handleFocusOut);
    window.addEventListener('focus', handleWindowFocus);
    window.addEventListener('blur', handleWindowBlur);

    Promise.all([loadGhostty(), ensureNerdFonts()]).then(([{ module, ghostty }]) => {
      if (disposed) return;
      terminal = new module.Terminal({
        ...getGhosttyTerminalOptions(fontFamily, fontSize, theme, ghostty, false),
        ...(provisionalSizeRef.current ?? {}),
      });
      const fitAddon = new module.FitAddon();
      terminal.loadAddon(fitAddon);
      terminal.open(container);
      // Ctrl+wheel view zoom: ghostty's capture-phase wheel handler
      // stopPropagation()s normal wheels, so the only reliable hook is its
      // custom handler. Return true to claim ctrl-wheels for VIEW zoom
      // (pure visual scale — grid and font untouched); everything else
      // falls through to ghostty's scroll handling.
      terminal.attachCustomWheelEventHandler((event) => {
        if (!event.ctrlKey) return false;
        const factor = event.deltaY < 0 ? 1.1 : 1 / 1.1;
        zoomApiRef.current?.setViewZoom(viewZoomRef.current * factor);
        return true;
      });
      terminalRef.current = terminal;
      fitRef.current = fitAddon;
      subscriptions = [terminal.onData((data) => inputRef.current(data))];
      observer = new ResizeObserver(() => {
        if (resizeTimeout) clearTimeout(resizeTimeout);
        resizeTimeout = setTimeout(fit, 80);
      });
      observer.observe(container);
      // Canvas-resize observer: the ghostty canvas resizes asynchronously
      // after terminal.resize(), so driver-follow scaling must re-run when
      // the canvas layout size actually settles (not just on requestAnimationFrame).
      const canvas = container.querySelector('canvas');
      if (canvas) {
        canvasObserver = new ResizeObserver(() => applyViewScale());
        canvasObserver.observe(canvas);
      }
      const safeReset = safeResetRef.current;
      if (safeReset) terminal.write(`${safeReset}\u001b[2J\u001b[H`);
      fitFrame = requestAnimationFrame(fit);
    });

    return () => {
      disposed = true;
      // Removing a focused editable mid-IME-composition wedges Android
      // WebView's input dispatch (the whole app stops responding to touch).
      // Blur first so the IME detaches cleanly, and hide the soft keyboard
      // explicitly on Android before the terminal DOM is torn down.
      const active = document.activeElement;
      if (active instanceof HTMLElement && container.contains(active)) {
        active.blur();
        const capacitor = (window as typeof window & { Capacitor?: { getPlatform?: () => string } }).Capacitor;
        if (capacitor?.getPlatform?.() === 'android') {
          void import('@capacitor/keyboard')
            .then(({ Keyboard }) => Keyboard.hide())
            .catch(() => undefined);
        }
      }
      observer?.disconnect();
      canvasObserver?.disconnect();
      if (resizeTimeout) clearTimeout(resizeTimeout);
      if (fitFrame !== null) cancelAnimationFrame(fitFrame);
      container.removeEventListener('focusin', handleFocusIn);
      container.removeEventListener('focusout', handleFocusOut);
      window.removeEventListener('focus', handleWindowFocus);
      window.removeEventListener('blur', handleWindowBlur);
      subscriptions.forEach((subscription) => subscription.dispose());
      terminal?.dispose();
      terminalRef.current = null;
      fitRef.current = null;
      lastSizeRef.current = null;
      lastChunkRef.current = null;
      writeQueueRef.current = '';
      outputRewriteCarryRef.current = '';
      writingRef.current = false;
      writeEpochRef.current += 1;
      rendererReadyRef.current = false;
    };
  }, [fit, fontFamily, fontSize, rendererGeneration, theme]);

  React.useEffect(() => {
    const terminal = terminalRef.current;
    const container = containerRef.current;
    if (!terminal || !container) return;
    terminal.options.cursorBlink = isVisible && document.hasFocus() && container.contains(document.activeElement);
  }, [isVisible, ready]);

  React.useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) return;
    if (chunks.length === 0) {
      if (lastChunkRef.current !== null) recreateRenderer();
      return;
    }
    const previous = lastChunkRef.current;
    // Chunk ids are monotonic and the store appends, so the already-written chunk
    // is normally the last one. Scanning from the end keeps this O(1) per chunk
    // instead of O(chunks) on every streamed write.
    let previousIndex = -1;
    if (previous !== null) {
      for (let index = chunks.length - 1; index >= 0; index -= 1) {
        const id = chunks[index].id;
        if (id === previous) { previousIndex = index; break; }
        if (id < previous) break;
      }
      if (previousIndex < 0) {
        // A snapshot replaced the buffer (ids no longer line up). Reset and
        // replay the new history in the same pass — returning early would
        // leave the screen blank until the next append.
        recreateRenderer();
      }
    }
    // After recreateRenderer, lastChunkRef is null, so re-read it: a null
    // value means full replay (uses replayData where provided).
    const isReplay = lastChunkRef.current === null;
    const pending = previousIndex >= 0 ? chunks.slice(previousIndex + 1) : chunks;
    if (pending.length > 0) {
      writeQueueRef.current += pending.map((chunk) => isReplay ? (chunk.replayData ?? chunk.data) : chunk.data).join('');
      const pendingSequence = pending.at(-1)?.sequence;
      if (pendingSequence !== undefined) queuedSequenceRef.current = pendingSequence;
    }
    lastChunkRef.current = chunks.at(-1)?.id ?? null;
    flush();
  }, [chunks, flush, ready, recreateRenderer]);
  React.useEffect(() => {
    if (!autoFocus || !isVisible) return;
    const frame = requestAnimationFrame(() => terminalRef.current?.focus());
    return () => cancelAnimationFrame(frame);
  }, [autoFocus, isVisible, ready, sessionKey]);

  React.useEffect(() => {
    const container = containerRef.current;
    if (!enableTouchScroll || !container) return;
    // ghostty-web only reads keydown/composition events and preventDefaults
    // beforeinput without consuming it. Android IMEs deliver text via
    // beforeinput (their keydown arrives as keyCode 229, which ghostty
    // ignores), so forward those payloads to the terminal here. Composition
    // updates are skipped: ghostty commits them itself on compositionend.
    const handleBeforeInput = (event: Event) => {
      const input = event as InputEvent;
      if (input.isComposing) return;
      switch (input.inputType) {
        case 'insertText':
          if (input.data) inputRef.current(input.data);
          break;
        case 'insertLineBreak':
        case 'insertParagraph':
          inputRef.current('\r');
          break;
        case 'deleteContentBackward':
          inputRef.current('\x7f');
          break;
        default:
          break;
      }
    };
    container.addEventListener('beforeinput', handleBeforeInput);
    return () => container.removeEventListener('beforeinput', handleBeforeInput);
  }, [enableTouchScroll, ready]);

  React.useEffect(() => {
    const container = containerRef.current;
    const terminal = terminalRef.current;
    if (!enableTouchScroll || !container || !terminal) return;
    let pointerId: number | null = null;
    // Second finger while the application tracks the mouse: two-finger pan
    // becomes wheel events instead of a second pointer gesture.
    let wheelPointerId: number | null = null;
    let longPressTimeout: ReturnType<typeof setTimeout> | null = null;
    type Gesture = 'idle' | 'pending' | 'scrolling' | 'selecting' | 'app-press' | 'app-wheel' | 'pinch';
    let gesture: Gesture = 'idle';
    let startX = 0;
    let startY = 0;
    let lastY = 0;
    let remainder = 0;
    let selectionFocus: TerminalCellPosition | null = null;
    // Application-mode tracking: last synthesized mouse coordinates and the
    // two-finger wheel baseline.
    let appX = 0;
    let appY = 0;
    let wheelBaseY = 0;
    // Pinch-to-zoom: pure VIEW zoom. The gesture previews the scale live;
    // release keeps the chosen multiplier (clamped). Grid and font size are
    // untouched — the negotiated grid stays authoritative for everyone.
    const pinchPointers = new Map<number, { x: number; y: number }>();
    let pinchBaseDistance = 0;
    let pinchScale = 1;
    const pinchZoomBase = viewZoomRef.current;
    const applyPinchPreview = () => {
      const clamped = Math.max(0.25, Math.min(4, pinchZoomBase * pinchScale));
      viewZoomRef.current = clamped;
      applyViewScale();
    };
    const finishPinch = () => {
      pinchPointers.clear();
      pinchBaseDistance = 0;
      pinchScale = 1;
      setViewZoom(viewZoomRef.current);
    };
    const lineHeight = Math.max(12, fontSize + 2);
    // Android WebView only raises the soft keyboard for a native tap-focus; the
    // pointer-captured, touch-action:none tap here focuses programmatically, so
    // the IME must be summoned explicitly via the Capacitor Keyboard plugin.
    const showAndroidSoftKeyboard = () => {
      const capacitor = (window as typeof window & { Capacitor?: { getPlatform?: () => string } }).Capacitor;
      if (capacitor?.getPlatform?.() !== 'android') return;
      void import('@capacitor/keyboard')
        .then(({ Keyboard }) => Keyboard.show())
        .catch(() => undefined);
    };
    const clearLongPress = () => {
      if (!longPressTimeout) return;
      clearTimeout(longPressTimeout);
      longPressTimeout = null;
    };
    const cellFromPoint = (clientX: number, clientY: number) => {
      const canvas = container.querySelector('canvas');
      if (!canvas) return null;
      return getTerminalCellFromPoint(clientX, clientY, canvas.getBoundingClientRect(), terminal.cols, terminal.rows);
    };
    const dispatchSelectionMouseEvent = (
      type: 'mousedown' | 'mousemove',
      cell: TerminalCellPosition,
    ) => {
      const canvas = container.querySelector('canvas');
      if (!canvas) return;
      const bounds = canvas.getBoundingClientRect();
      const clientX = bounds.left + ((cell.column + 0.5) / terminal.cols) * bounds.width;
      const clientY = bounds.top + ((cell.row + 0.5) / terminal.rows) * bounds.height;
      canvas.dispatchEvent(new MouseEvent(type, {
        bubbles: true,
        cancelable: true,
        button: 0,
        buttons: 1,
        clientX,
        clientY,
      }));
    };
    const finishSelection = () => {
      document.dispatchEvent(new MouseEvent('mouseup', {
        bubbles: true,
        cancelable: true,
        button: 0,
        buttons: 0,
      }));
    };
    // Application-mode synthesis: the TUI owns the pointer while it enables
    // mouse reporting, so touch becomes mouse — tap = click, drag = drag,
    // two-finger pan = wheel. Events are dispatched at the canvas so they
    // ride the exact mouse pipeline the desktop path uses.
    const dispatchAppMouseEvent = (
      type: 'mousedown' | 'mousemove' | 'mouseup',
      x: number,
      y: number,
    ) => {
      const canvas = container.querySelector('canvas');
      if (!canvas) return;
      canvas.dispatchEvent(new MouseEvent(type, {
        bubbles: true,
        cancelable: true,
        button: 0,
        buttons: type === 'mouseup' ? 0 : 1,
        clientX: x,
        clientY: y,
      }));
    };
    const dispatchAppWheel = (lines: number) => {
      container.dispatchEvent(new WheelEvent('wheel', {
        bubbles: true,
        cancelable: true,
        deltaMode: 0,
        // 33px per notch matches InputHandler's pixel-mode wheel divisor.
        deltaY: lines * 33,
        clientX: appX,
        clientY: appY,
      }));
    };
    const down = (event: PointerEvent) => {
      if (event.pointerType !== 'touch') return;
      // Second finger during an application gesture turns the pan into a
      // wheel scroll; close the accidental press first so the application
      // sees a clean click-or-drag, not a held button.
      if (wheelPointerId === null && pointerId !== null && event.pointerId !== pointerId &&
          (gesture === 'app-press' || gesture === 'app-wheel')) {
        if (gesture === 'app-press') dispatchAppMouseEvent('mouseup', appX, appY);
        wheelPointerId = event.pointerId;
        wheelBaseY = event.clientY;
        gesture = 'app-wheel';
        return;
      }
      if (pointerId !== null) {
        // Second finger outside application mode → pinch-to-zoom.
        if (gesture !== 'app-press' && gesture !== 'app-wheel' && event.pointerId !== pointerId) {
          clearLongPress();
          if (gesture === 'selecting') finishSelection();
          pinchPointers.set(pointerId, { x: startX, y: startY });
          pinchPointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
          const [a, b] = [...pinchPointers.values()];
          pinchBaseDistance = Math.hypot(a.x - b.x, a.y - b.y) || 1;
          pinchScale = 1;
          try { container.releasePointerCapture(pointerId); } catch { /* already released */ }
          pointerId = null;
          gesture = 'pinch';
        }
        return;
      }
      pointerId = event.pointerId;
      startX = event.clientX;
      startY = event.clientY;
      lastY = event.clientY;
      remainder = 0;
      selectionFocus = null;
      container.setPointerCapture(event.pointerId);
      if (terminal.hasMouseTracking()) {
        gesture = 'app-press';
        appX = event.clientX;
        appY = event.clientY;
        dispatchAppMouseEvent('mousedown', appX, appY);
        return;
      }
      gesture = 'pending';
      longPressTimeout = setTimeout(() => {
        longPressTimeout = null;
        if (pointerId !== event.pointerId || gesture !== 'pending') return;
        const cell = cellFromPoint(startX, startY);
        if (!cell) return;

        const buffer = terminal.buffer.active;
        const lineIndex = Math.max(0, buffer.length - terminal.rows - buffer.viewportY + cell.row);
        const line = buffer.getLine(lineIndex);
        const cells = Array.from({ length: terminal.cols }, (_, column) => line?.getCell(column)?.getChars() ?? '');
        const word = getTerminalWordRange(cells, cell.column);
        const selectionAnchor = { column: word.startColumn, row: cell.row };
        selectionFocus = { column: word.endColumn, row: cell.row };
        gesture = 'selecting';
        dispatchSelectionMouseEvent('mousedown', selectionAnchor);
        dispatchSelectionMouseEvent('mousemove', selectionFocus);
      }, 350);
    };
    const move = (event: PointerEvent) => {
      if (event.pointerType !== 'touch') return;

      if (gesture === 'app-wheel') {
        const pointer = wheelPointerId === event.pointerId ? event : (pointerId === event.pointerId ? event : null);
        if (!pointer) return;
        if (wheelPointerId === event.pointerId) {
          remainder += wheelBaseY - event.clientY;
          wheelBaseY = event.clientY;
          const lines = Math.trunc(remainder / lineHeight);
          if (lines) {
            dispatchAppWheel(lines);
            remainder -= lines * lineHeight;
          }
        }
        if (event.cancelable) event.preventDefault();
        return;
      }

      if (gesture === 'pinch') {
        const tracked = pinchPointers.get(event.pointerId);
        if (!tracked) return;
        tracked.x = event.clientX;
        tracked.y = event.clientY;
        const [a, b] = [...pinchPointers.values()];
        if (pinchPointers.size >= 2) {
          const distance = Math.hypot(a.x - b.x, a.y - b.y);
          pinchScale = Math.max(0.4, Math.min(3, distance / pinchBaseDistance));
          applyPinchPreview();
        }
        if (event.cancelable) event.preventDefault();
        return;
      }

      if (pointerId !== event.pointerId) return;

      if (gesture === 'app-press') {
        appX = event.clientX;
        appY = event.clientY;
        dispatchAppMouseEvent('mousemove', appX, appY);
        if (event.cancelable) event.preventDefault();
        return;
      }

      if (gesture === 'selecting') {
        const focus = cellFromPoint(event.clientX, event.clientY);
        if (focus && (!selectionFocus || focus.column !== selectionFocus.column || focus.row !== selectionFocus.row)) {
          selectionFocus = focus;
          dispatchSelectionMouseEvent('mousemove', focus);
        }
        if (event.cancelable) event.preventDefault();
        return;
      }

      if (gesture === 'pending') {
        const distance = Math.hypot(event.clientX - startX, event.clientY - startY);
        if (distance < 8) return;
        clearLongPress();
        gesture = 'scrolling';
      }

      if (gesture !== 'scrolling') return;
      const delta = lastY - event.clientY;
      lastY = event.clientY;
      remainder += delta;
      const lines = Math.trunc(remainder / lineHeight);
      if (lines) { terminal.scrollLines(lines); remainder -= lines * lineHeight; }
      if (event.cancelable) event.preventDefault();
    };
    const releasePointer = (event: PointerEvent) => {
      clearLongPress();
      if (container.hasPointerCapture(event.pointerId)) container.releasePointerCapture(event.pointerId);
    };
    const up = (event: PointerEvent) => {
      if (event.pointerType !== 'touch') return;

      if (gesture === 'app-wheel') {
        if (wheelPointerId === event.pointerId) {
          releasePointer(event);
          wheelPointerId = null;
        } else if (pointerId === event.pointerId) {
          releasePointer(event);
          pointerId = null;
        }
        if (wheelPointerId === null && pointerId === null) gesture = 'idle';
        return;
      }

      if (gesture === 'pinch') {
        pinchPointers.delete(event.pointerId);
        if (pinchPointers.size <= 1) {
          gesture = 'idle';
          finishPinch();
        }
        return;
      }

      if (pointerId !== event.pointerId) return;
      const shouldFocus = gesture === 'pending';
      const shouldFinishSelection = gesture === 'selecting';
      const wasAppPress = gesture === 'app-press';
      releasePointer(event);
      pointerId = null;
      gesture = 'idle';
      if (shouldFinishSelection) finishSelection();
      if (wasAppPress) dispatchAppMouseEvent('mouseup', appX, appY);
      if (shouldFocus) {
        terminal.focus();
        showAndroidSoftKeyboard();
      }
    };
    const cancel = (event: PointerEvent) => {
      if (event.pointerType !== 'touch') return;

      if (gesture === 'app-wheel') {
        if (wheelPointerId === event.pointerId) {
          releasePointer(event);
          wheelPointerId = null;
        } else if (pointerId === event.pointerId) {
          releasePointer(event);
          pointerId = null;
        }
        if (wheelPointerId === null && pointerId === null) gesture = 'idle';
      }

      if (gesture === 'pinch') {
        pinchPointers.delete(event.pointerId);
        if (pinchPointers.size <= 1) {
          gesture = 'idle';
          pinchScale = 1;
          applyViewScale();
        }
        return;
      }

      if (pointerId !== event.pointerId) return;
      const shouldFinishSelection = gesture === 'selecting';
      const wasAppPress = gesture === 'app-press';
      releasePointer(event);
      pointerId = null;
      gesture = 'idle';
      if (shouldFinishSelection) finishSelection();
      if (wasAppPress) dispatchAppMouseEvent('mouseup', appX, appY);
    };
    container.addEventListener('pointerdown', down);
    container.addEventListener('pointermove', move, { passive: false });
    container.addEventListener('pointerup', up);
    container.addEventListener('pointercancel', cancel);
    return () => {
      clearLongPress();
      container.removeEventListener('pointerdown', down);
      container.removeEventListener('pointermove', move);
      container.removeEventListener('pointerup', up);
      container.removeEventListener('pointercancel', cancel);
    };
  }, [enableTouchScroll, fontSize, ready]);



  React.useImperativeHandle(ref, () => ({
    focus: () => terminalRef.current?.focus(),
    fit,
    resizeGrid: (rawCols: number, rawRows: number) => {
      const terminal = terminalRef.current;
      if (!terminal) return;
      // A degenerate grid (0/1 cols) reaching the WASM terminal corrupts it
      // (out-of-bounds traps deep in the VT parser), so clamp before resize.
      const cols = Math.max(2, Math.min(1000, Math.floor(rawCols)));
      const rows = Math.max(2, Math.min(500, Math.floor(rawRows)));
      if (!Number.isFinite(cols) || !Number.isFinite(rows)) return;
      negotiatedGridRef.current = { cols, rows };
      if (terminal.cols === cols && terminal.rows === rows) {
        requestAnimationFrame(() => applyViewScale());
        return;
      }
      terminal.resize(cols, rows);
      requestAnimationFrame(() => applyViewScale());
    },
    zoomIn: () => setViewZoom(viewZoomRef.current * 1.25),
    zoomOut: () => setViewZoom(viewZoomRef.current / 1.25),
    zoomReset: () => setViewZoom(1),
    getZoom: () => viewZoomRef.current,
    getSelection: () => {
      const terminal = terminalRef.current;
      const range = terminal?.getSelectionPosition();
      const text = terminal?.getSelection() ?? '';
      if (!range || !text.trim()) return null;
      return { text, startLine: range.start.y + 1, endLine: range.end.y + 1 };
    },
    writeDirect: (data: string, sequence?: number) => {
      if (!data) return;
      if (sequence !== undefined) queuedSequenceRef.current = sequence;
      writeQueueRef.current += data;
      flush();
    },
  }), [fit, applyViewScale, setViewZoom, flush]);

  React.useEffect(() => { onZoomChange?.(viewZoom); }, [onZoomChange, viewZoom]);

  return (
    <div
      ref={containerRef}
      data-terminal-owner="main"
      className={cn('terminal-viewport-container h-full w-full overflow-hidden touch-none', className)}
    />
  );
});

TerminalViewport.displayName = 'TerminalViewport';
export { TerminalViewport };
