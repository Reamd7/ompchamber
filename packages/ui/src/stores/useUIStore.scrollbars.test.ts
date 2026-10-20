import { afterEach, describe, expect, test } from 'bun:test';
import { useUIStore } from './useUIStore';

const originalOptions = useUIStore.persist.getOptions();
const originalState = useUIStore.getState();

afterEach(() => {
  useUIStore.persist.setOptions(originalOptions);
  useUIStore.setState(originalState, true);
});

describe('scrollbar preference', () => {
  test('defaults to auto-hide when an existing install has no preference', async () => {
    expect(useUIStore.getInitialState().alwaysShowScrollbars).toBe(false);
    useUIStore.persist.setOptions({ storage: {
      getItem: () => ({ version: originalOptions.version, state: { dockBadgeEnabled: false } }),
      setItem: () => undefined,
      removeItem: () => undefined,
    } });
    useUIStore.setState(useUIStore.getInitialState(), true);
    await useUIStore.persist.rehydrate();
    expect(useUIStore.getState().alwaysShowScrollbars).toBe(false);
    expect(useUIStore.getState().dockBadgeEnabled).toBe(false);
  });

  for (const enabled of [true, false]) {
    test(`round-trips ${enabled} through the persisted store`, async () => {
      let saved: Parameters<NonNullable<typeof originalOptions.storage>['setItem']>[1] = {
        state: useUIStore.getInitialState(), version: originalOptions.version,
      };
      useUIStore.persist.setOptions({ storage: {
        getItem: () => saved,
        setItem: (_name, value) => { saved = value; },
        removeItem: () => undefined,
      } });
      useUIStore.getState().setAlwaysShowScrollbars(enabled);
      useUIStore.persist.setOptions({ storage: {
        getItem: () => saved,
        setItem: () => undefined,
        removeItem: () => undefined,
      } });
      useUIStore.getState().setAlwaysShowScrollbars(!enabled);
      await useUIStore.persist.rehydrate();
      expect(useUIStore.getState().alwaysShowScrollbars).toBe(enabled);
    });
  }

  test('stays local to the device via the persisted store', () => {
    // The fork has no settings registry; device-local persistence is the
    // zustand persist partialize, and the key must never reach server sync.
    const original = useUIStore.getState();
    useUIStore.setState({ alwaysShowScrollbars: true });
    const partialized = (useUIStore.persist.getOptions().partialize as (state: typeof original) => unknown)(useUIStore.getState());
    expect((partialized as Record<string, unknown>).alwaysShowScrollbars).toBe(true);
    useUIStore.setState(original, true);
  });
});
