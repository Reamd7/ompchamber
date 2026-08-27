/**
 * Clamp the app shell to the visual viewport while iOS reserves keyboard
 * space for a hardware keyboard's minimized on-screen widget.
 *
 * With a hardware keyboard attached (Magic Keyboard et al.), focusing a
 * text field on iOS/iPadOS reserves a bottom strip for the minimized
 * on-screen keyboard widget. WebKit takes that strip only from the VISUAL
 * viewport — the layout viewport (and 100dvh) keeps full height — so a
 * full-height shell extends under the strip: the strip renders as dead
 * non-page space and the window stays scrollable into it (the stray
 * scrollbar). Sizing the shell to the actually visible height removes
 * both. Same class of fix as VS Code's iOS workbench sizing, which trusts
 * `visualViewport` over `innerHeight` (microsoft/vscode#122390).
 *
 * The RESERVED GEOMETRY is the trigger, not focus state: iPadOS 26 keeps
 * the widget strip after blur, and WebKit does not reliably fire
 * visualViewport resize when the strip appears (bugs.webkit.org 198347),
 * so a focus-gated, event-driven clamp can miss both edges. Events remain
 * fast-path triggers; an rAF loop (only while an editable is focused or
 * the clamp is active) covers missing events.
 *
 * Scope guard: a REAL software keyboard reserves far more than the widget
 * strip, and those flows already have their own handling
 * (interactive-widget / dvh in the browser, the Capacitor keyboard
 * choreography). Only strips shorter than a real software keyboard — the
 * widget band — engage the clamp, so no existing keyboard behavior changes.
 */

import React from 'react';

import { SOFTWARE_KEYBOARD_MIN_HEIGHT_PX } from '@/lib/hardwareKeyboard';

/** Below this a reserved strip is rounding/safe-area noise, not the widget. */
const CLAMP_MIN_STRIP_PX = 16;

/** Root class + CSS variable the shells consume (see mobile.css). */
const CLAMP_CLASS = 'oc-vv-clamp';
const CLAMP_HEIGHT_VAR = '--oc-vv-clamp-height';

/**
 * True on iOS/iPadOS WebKit in any surface: browser tab, standalone PWA, or
 * WKWebView. iPadOS reports a desktop Mac UA, so touch points are the tell.
 */
const isIOSWebKitRuntime = (): boolean => {
  const userAgent = navigator.userAgent || '';
  const maxTouchPoints = navigator.maxTouchPoints ?? 0;
  return /iPhone|iPad|iPod/i.test(userAgent)
    || (/Macintosh|MacIntel/i.test(userAgent) && maxTouchPoints > 1);
};

export interface ViewportClampInput {
  /** `visualViewport.height` in CSS pixels. */
  visualHeight: number;
  /** `visualViewport.offsetTop` in CSS pixels (top chrome / pan). */
  visualOffsetTop: number;
  /** `document.documentElement.clientHeight` — the layout viewport height. */
  layoutHeight: number;
  /** `visualViewport.scale` — pinch zoom also shrinks the visual viewport. */
  scale: number;
}

export interface ViewportClampResult {
  /** Whether the shell should be clamped to the visible height. */
  active: boolean;
  /** The visible height in whole CSS pixels, or null when inactive. */
  heightPx: number | null;
}

/**
 * Decide the clamp from viewport measurements. Pure: every WebKit timing
 * quirk lives in the hook below, every geometry decision lives here.
 *
 * No focus term on purpose: the geometry IS the question. At scale 1 the
 * only thing that shrinks the visual viewport without shrinking the layout
 * viewport is reserved keyboard space (browser chrome moves both, pinch
 * zoom is excluded by the scale guard) — so a widget-band strip clamps
 * even while the widget lingers after blur.
 */
export const resolveIOSKeyboardViewportClamp = (input: ViewportClampInput): ViewportClampResult => {
  // Pinch zoom shrinks the visual viewport the same way — that loss is the
  // user's zoom, not keyboard space, and must not resize the shell.
  if (input.scale > 1.02) {
    return { active: false, heightPx: null };
  }
  // Stale-visualViewport guard (iOS standalone serves pre-keyboard full
  // height intermittently): the visible bottom never exceeds the layout
  // viewport. Mirrors useMobileViewportPin's min() anchoring.
  const visibleBottom = Math.min(input.visualOffsetTop + input.visualHeight, input.layoutHeight);
  const strip = input.layoutHeight - visibleBottom;
  if (strip < CLAMP_MIN_STRIP_PX || strip >= SOFTWARE_KEYBOARD_MIN_HEIGHT_PX) {
    return { active: false, heightPx: null };
  }
  return { active: true, heightPx: Math.round(visibleBottom) };
};

const isEditableElement = (node: Element | null): node is HTMLElement =>
  node instanceof HTMLElement
  && (node.tagName === 'TEXTAREA' || node.tagName === 'INPUT' || node.isContentEditable);

/**
 * Keep `--oc-vv-clamp-height` / `.oc-vv-clamp` on <html> in step with the
 * reserved widget strip. Mount once per app root (SyncAppEffects).
 *
 * Event coverage: focusin/focusout, visualViewport resize/scroll, window
 * resize/orientationchange. Frame coverage: an rAF loop that runs only
 * while an editable is focused OR the clamp is active, because WebKit can
 * skip the visualViewport events when the widget strip appears
 * (bugs.webkit.org 198347). Both paths share one evaluate; style writes
 * are change-gated, so the loop is a few reads per frame and a write only
 * when the geometry actually moves.
 */
export function useIOSKeyboardViewportClamp(): void {
  React.useEffect(() => {
    if (!isIOSWebKitRuntime()) return;
    const vv = window.visualViewport;
    if (!vv) return;

    const root = document.documentElement;
    let lastActive = false;
    let lastHeight: number | null = null;
    let frame = 0;
    let loopWanted = false;

    const evaluate = () => {
      const { active, heightPx } = resolveIOSKeyboardViewportClamp({
        visualHeight: vv.height,
        visualOffsetTop: vv.offsetTop,
        layoutHeight: root.clientHeight,
        scale: vv.scale,
      });
      if (active !== lastActive) {
        lastActive = active;
        root.classList.toggle(CLAMP_CLASS, active);
        if (active) {
          // Engage from a panned state (Safari scrolled the full-height
          // shell into the strip): snap the pan back first, then measure —
          // the clamped document fits, so there is nothing to scroll into.
          if (window.scrollY > 0) {
            window.scrollTo(0, 0);
            return evaluate();
          }
        }
      }
      if (heightPx !== lastHeight) {
        lastHeight = heightPx;
        if (heightPx === null) {
          root.style.removeProperty(CLAMP_HEIGHT_VAR);
        } else {
          root.style.setProperty(CLAMP_HEIGHT_VAR, `${heightPx}px`);
        }
      }
      return { active };
    };

    // The loop runs only while it can matter: an editable focused (strip
    // may appear/disappear) or the clamp active (strip may vanish — widget
    // dismissed, keyboard detached, Split View resize). Otherwise zero cost.
    const track = (): void => {
      const editableFocused = isEditableElement(document.activeElement);
      const { active } = evaluate();
      loopWanted = editableFocused || active;
      if (loopWanted) {
        frame = requestAnimationFrame(track);
      } else {
        frame = 0;
      }
    };
    const kickLoop = (): void => {
      if (frame === 0) {
        frame = requestAnimationFrame(track);
      }
    };
    // Track while focused even before any strip exists: the loop is what
    // catches a silent strip appearance when vv events never fire.
    const onFocusIn = (): void => {
      if (isEditableElement(document.activeElement)) kickLoop();
    };

    evaluate();
    kickLoop();
    document.addEventListener('focusin', onFocusIn, true);
    document.addEventListener('focusout', kickLoop, true);
    vv.addEventListener('resize', kickLoop);
    vv.addEventListener('scroll', kickLoop);
    window.addEventListener('resize', kickLoop);
    window.addEventListener('orientationchange', kickLoop);

    return () => {
      if (frame !== 0) {
        cancelAnimationFrame(frame);
        frame = 0;
      }
      document.removeEventListener('focusin', onFocusIn, true);
      document.removeEventListener('focusout', kickLoop, true);
      vv.removeEventListener('resize', kickLoop);
      vv.removeEventListener('scroll', kickLoop);
      window.removeEventListener('resize', kickLoop);
      window.removeEventListener('orientationchange', kickLoop);
      root.classList.remove(CLAMP_CLASS);
      root.style.removeProperty(CLAMP_HEIGHT_VAR);
    };
  }, []);
}
