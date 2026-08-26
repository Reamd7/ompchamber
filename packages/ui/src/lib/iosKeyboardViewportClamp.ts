/**
 * Clamp the app shell to the visual viewport while iOS reserves keyboard
 * space for a hardware keyboard's minimized on-screen widget.
 *
 * With a hardware keyboard attached (Magic Keyboard et al.), focusing a
 * text field on iOS/iPadOS still reserves a bottom strip for the minimized
 * on-screen keyboard widget. WebKit takes that strip only from the VISUAL
 * viewport — the layout viewport (and 100dvh) keeps full height — so a
 * full-height shell extends under the strip: the strip renders as dead
 * non-page space and the window stays scrollable into it (the stray
 * scrollbar). Sizing the shell to the actually visible height removes
 * both. Same class of fix as VS Code's iOS workbench sizing, which trusts
 * `visualViewport` over `innerHeight` (microsoft/vscode#122390).
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
  if (typeof navigator === 'undefined') return false;
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
  /** A text-editable element currently holds focus (the widget is up). */
  editableFocused: boolean;
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
 */
export const resolveIOSKeyboardViewportClamp = (input: ViewportClampInput): ViewportClampResult => {
  if (!input.editableFocused) {
    return { active: false, heightPx: null };
  }
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

const isEditableElement = (node: unknown): node is HTMLElement =>
  node instanceof HTMLElement
  && (node.tagName === 'TEXTAREA' || node.tagName === 'INPUT' || node.isContentEditable);

/**
 * Keep `--oc-vv-clamp-height` / `.oc-vv-clamp` on <html> in step with the
 * reserved widget strip. Mount once per app root (SyncAppEffects).
 *
 * The style writes are change-gated: events fire in bursts while the widget
 * appears and while Safari pans, but each evaluate is a few reads plus at
 * most one toggle and one property write.
 */
export function useIOSKeyboardViewportClamp(): void {
  React.useEffect(() => {
    if (!isIOSWebKitRuntime()) return;
    const vv = window.visualViewport;
    if (!vv) return;

    const root = document.documentElement;
    let lastActive = false;
    let lastHeight: number | null = null;
    let releaseTimer: number | null = null;

    const clearReleaseTimer = () => {
      if (releaseTimer !== null) {
        window.clearTimeout(releaseTimer);
        releaseTimer = null;
      }
    };

    const evaluate = () => {
      const { active, heightPx } = resolveIOSKeyboardViewportClamp({
        visualHeight: vv.height,
        visualOffsetTop: vv.offsetTop,
        layoutHeight: root.clientHeight,
        scale: vv.scale,
        editableFocused: isEditableElement(document.activeElement),
      });
      if (active !== lastActive) {
        lastActive = active;
        root.classList.toggle(CLAMP_CLASS, active);
      }
      if (heightPx !== lastHeight) {
        lastHeight = heightPx;
        if (heightPx === null) {
          root.style.removeProperty(CLAMP_HEIGHT_VAR);
        } else {
          root.style.setProperty(CLAMP_HEIGHT_VAR, `${heightPx}px`);
        }
      }
    };

    const evaluateOnFocusIn = () => {
      // Focus moving directly between two inputs fires focusout → focusin
      // before paint; cancel the pending release so the clamp holds.
      clearReleaseTimer();
      evaluate();
    };

    const evaluateOnFocusOut = () => {
      // Deferred like the Capacitor bridge's focusout (mobileNativeChrome):
      // at timeout, activeElement is the final target — a plain blur clears
      // the clamp, a same-frame refocus (canceled above) keeps it.
      clearReleaseTimer();
      releaseTimer = window.setTimeout(evaluate, 0);
    };

    evaluate();
    document.addEventListener('focusin', evaluateOnFocusIn, true);
    document.addEventListener('focusout', evaluateOnFocusOut, true);
    vv.addEventListener('resize', evaluate);
    vv.addEventListener('scroll', evaluate);
    window.addEventListener('resize', evaluate);
    window.addEventListener('orientationchange', evaluate);

    return () => {
      clearReleaseTimer();
      document.removeEventListener('focusin', evaluateOnFocusIn, true);
      document.removeEventListener('focusout', evaluateOnFocusOut, true);
      vv.removeEventListener('resize', evaluate);
      vv.removeEventListener('scroll', evaluate);
      window.removeEventListener('resize', evaluate);
      window.removeEventListener('orientationchange', evaluate);
      root.classList.remove(CLAMP_CLASS);
      root.style.removeProperty(CLAMP_HEIGHT_VAR);
    };
  }, []);
}
