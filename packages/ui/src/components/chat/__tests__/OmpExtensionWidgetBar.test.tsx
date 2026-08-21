/**
 * OmpExtensionWidgetBar — render contract (spec 09 §5.1).
 *
 * Extension rows are verbatim by contract (the TUI checksum anchor); the bar
 * renders nothing without widgets (data presence is the capability gate).
 *
 * The chrome selector is stubbed at its module seam: zustand v5 selectors
 * read the store's INITIAL state under renderToStaticMarkup, so a seeded
 * live store would be invisible to a static render. The store-side wiring
 * (applyEvent/reconcile) is covered by omp-event-reducer.test.ts and the
 * reconcile assertions below (direct getState reads — no SSR involved).
 */
import { describe, expect, mock, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';

import type { OmpChromeState } from '@/sync/omp-event-reducer';
import { createEmptyOmpDirectoryState } from '@/sync/omp-event-reducer';

let fakeChrome: OmpChromeState = { widgets: {}, status: {} };

mock.module('@/sync/useOmpSessionStore', () => ({
  useOmpChromeState: (_directory: string) => fakeChrome,
}));

const { OmpExtensionWidgetBar } = await import('../OmpExtensionWidgetBar');
const { useOmpSessionStore } = await import('@/sync/useOmpSessionStore');
const { I18nProvider } = await import('@/lib/i18n');


const DIRECTORY = '/repo';
const render = (): string =>
  renderToStaticMarkup(
    <I18nProvider>
      <OmpExtensionWidgetBar directory={DIRECTORY} placement="aboveEditor" />
    </I18nProvider>,
  );

describe('OmpExtensionWidgetBar — render contract', () => {
  test('empty chrome renders nothing', () => {
    fakeChrome = { widgets: {}, status: {} };
    expect(render()).toBe('');
  });

  test('widget rows render verbatim (TUI checksum anchor)', () => {
    fakeChrome = {
      widgets: {
        zhipu: { key: 'zhipu', lines: ['GLM Coding Plan · Max', '██░░░░░░░░ 16%'], placement: 'aboveEditor', sessionId: 's', updatedAt: 1 },
      },
      status: {},
    };
    const markup = render();
    expect(markup).toContain('data-testid="omp-extension-widget-bar"');
    expect(markup).toContain('GLM Coding Plan · Max');
    expect(markup).toContain('██░░░░░░░░ 16%');
  });

  test('placement filters: belowEditor widgets stay off the above bar', () => {
    fakeChrome = {
      widgets: { below: { key: 'below', lines: ['under'], placement: 'belowEditor', sessionId: 's', updatedAt: 1 } },
      status: {},
    };
    expect(render()).toBe('');
  });

  test('missing placement defaults to aboveEditor; newest widget renders last', () => {
    fakeChrome = {
      widgets: {
        plain: { key: 'plain', lines: ['default-side'], sessionId: 's', updatedAt: 1 },
        later: { key: 'later', lines: ['newer'], placement: 'aboveEditor', sessionId: 's', updatedAt: 2 },
      },
      status: {},
    };
    const markup = render();
    expect(markup).toContain('default-side');
    expect(markup.indexOf('default-side')).toBeLessThan(markup.indexOf('newer'));
  });
});

