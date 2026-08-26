import { describe, expect, test } from 'bun:test';

import { resolveIOSKeyboardViewportClamp } from './iosKeyboardViewportClamp';

// Geometry-only contract: the WebKit event wiring around it is not
// meaningfully testable off-device (same stance as useMobileViewportPin).
// Layout 820 / visual 760 = the ~60px strip iPadOS reserves for the
// minimized keyboard widget while a hardware keyboard is attached.
const widgetStrip = (overrides: Partial<Parameters<typeof resolveIOSKeyboardViewportClamp>[0]> = {}) => ({
  visualHeight: 760,
  visualOffsetTop: 0,
  layoutHeight: 820,
  scale: 1,
  editableFocused: true,
  ...overrides,
});

describe('resolveIOSKeyboardViewportClamp', () => {
  test('a widget-band strip with an editable focused clamps to the visible height', () => {
    expect(resolveIOSKeyboardViewportClamp(widgetStrip())).toEqual({ active: true, heightPx: 760 });
  });

  test('no editable focus means no clamp, whatever the viewport says', () => {
    expect(resolveIOSKeyboardViewportClamp(widgetStrip({ editableFocused: false }))).toEqual({
      active: false,
      heightPx: null,
    });
  });

  test('top chrome offsets count: clamp to offsetTop + height, not raw height', () => {
    expect(
      resolveIOSKeyboardViewportClamp(widgetStrip({ visualOffsetTop: 60, visualHeight: 720 })),
    ).toEqual({ active: true, heightPx: 780 });
  });

  test('sub-strip loss is rounding noise, not the widget', () => {
    expect(resolveIOSKeyboardViewportClamp(widgetStrip({ visualHeight: 812 }))).toEqual({
      active: false,
      heightPx: null,
    });
  });

  test('a full software keyboard reserve is out of scope: existing keyboard flows own it', () => {
    expect(resolveIOSKeyboardViewportClamp(widgetStrip({ visualHeight: 500 }))).toEqual({
      active: false,
      heightPx: null,
    });
  });

  test('a stale full-height visual viewport never clamps (standalone stale-metrics guard)', () => {
    expect(resolveIOSKeyboardViewportClamp(widgetStrip({ visualHeight: 820 }))).toEqual({
      active: false,
      heightPx: null,
    });
    // Even beyond the layout viewport, the visible bottom is capped:
    // strip computes as zero, not negative.
    expect(resolveIOSKeyboardViewportClamp(widgetStrip({ visualHeight: 900 }))).toEqual({
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
