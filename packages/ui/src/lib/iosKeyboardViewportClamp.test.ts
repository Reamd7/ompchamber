import { describe, expect, test } from 'bun:test';

import { resolveIOSKeyboardViewportClamp } from './iosKeyboardViewportClamp';

// Geometry-only contract: the resolver has no focus term on purpose — the
// reserved strip itself is the trigger (iPadOS 26 keeps the widget after
// blur), and the WebKit event wiring around it is not meaningfully testable
// off-device (same stance as useMobileViewportPin).
// Numbers below are the real on-device captures from iPadOS 26 + Magic
// Keyboard (layout 688, visual 618 = the ~70px minimized-widget strip).
const widgetStrip = (overrides: Partial<Parameters<typeof resolveIOSKeyboardViewportClamp>[0]> = {}) => ({
  visualHeight: 618,
  layoutHeight: 688,
  scale: 1,
  ...overrides,
});

describe('resolveIOSKeyboardViewportClamp', () => {
  test('a widget-band strip clamps to the visible height', () => {
    expect(resolveIOSKeyboardViewportClamp(widgetStrip())).toEqual({ active: true, heightPx: 618 });
  });

  test('regression: a panned visual viewport keeps the strip (scrollY=71, top=70 capture)', () => {
    // The first implementation subtracted offsetTop + height from the layout
    // height, which cancelled the strip exactly while Safari had scrolled
    // the page into the dead space — the clamp released mid-pan. The pan is
    // scroll, not reservation; only the height difference counts.
    expect(resolveIOSKeyboardViewportClamp(widgetStrip({ visualHeight: 618 }))).toEqual({
      active: true,
      heightPx: 618,
    });
  });

  test('sub-strip loss is rounding noise, not the widget', () => {
    expect(resolveIOSKeyboardViewportClamp(widgetStrip({ visualHeight: 680 }))).toEqual({
      active: false,
      heightPx: null,
    });
  });

  test('a full software keyboard reserve is out of scope: existing keyboard flows own it', () => {
    expect(resolveIOSKeyboardViewportClamp(widgetStrip({ visualHeight: 400 }))).toEqual({
      active: false,
      heightPx: null,
    });
  });

  test('a stale full-height visual viewport never clamps (standalone stale-metrics guard)', () => {
    expect(resolveIOSKeyboardViewportClamp(widgetStrip({ visualHeight: 688 }))).toEqual({
      active: false,
      heightPx: null,
    });
    // Even beyond the layout viewport, the height is capped: strip computes
    // as zero, not negative.
    expect(resolveIOSKeyboardViewportClamp(widgetStrip({ visualHeight: 760 }))).toEqual({
      active: false,
      heightPx: null,
    });
  });

  test('pinch-zoom visual viewport loss is zoom, not keyboard space', () => {
    expect(resolveIOSKeyboardViewportClamp(widgetStrip({ scale: 1.5 }))).toEqual({
      active: false,
      heightPx: null,
    });
  });
});
