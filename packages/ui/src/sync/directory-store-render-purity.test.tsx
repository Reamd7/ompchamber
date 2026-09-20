/**
 * `useDirectoryStore` render purity — regression coverage for the render-phase
 * updates React reported in bug.logs:
 *
 *   Cannot update a component (`SessionGroupSectionBase`) while rendering a
 *   different component (`RevertedMessageDock` / `ComposerStatusBar`)
 *
 * Those components read the directory store during render (`useDirectorySync`),
 * and the hook forwarded its bootstrap request straight into `ensureChild`.
 * That request publishes twice before the render finishes: `queueBootstrap`
 * notifies bootstrap subscribers synchronously (SessionGroupSectionBase
 * subscribes to exactly that), and the pump invokes `onBootstrap`, whose first
 * `bootstrapDirectory` commit lands before its first await — a `setState` on a
 * store other components are subscribed to.
 *
 * Contract: rendering may ensure the store exists, but must not start (or
 * publish) directory bootstrap work. The request is issued once the render
 * commits, and the bootstrap still runs.
 */
import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { Window } from 'happy-dom';
import React, { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';

import { ChildStoreManager } from './child-store';
import { useDirectorySync } from './sync-context';

const DIRECTORY = '/workspace';

type SyncRuntimeContext = { childStores: ChildStoreManager; currentDirectory: { get: () => string; subscribe: () => () => void } };

// SAFETY: sync-context.tsx publishes its runtime context on globalThis so every
// module instance shares one context identity; the cast only adds that key to
// the global object type and the guard below re-checks presence.
const runtimeContext = (globalThis as {
  __ompchamber_sync_runtime_context__?: React.Context<SyncRuntimeContext | null>;
}).__ompchamber_sync_runtime_context__;

if (!runtimeContext) {
  throw new Error('sync runtime context was not published on globalThis by @/sync/sync-context');
}

const RuntimeProvider = runtimeContext.Provider;

describe('useDirectoryStore render purity', () => {
  let root: Root;
  let mounted = false;
  let childStores: ChildStoreManager;
  let bootstrapped: string[];
  let duringRender: (string | undefined)[];

  const Probe = () => {
    const sessionCount = useDirectorySync(React.useCallback((state) => state.session.length, []), DIRECTORY);
    duringRender.push(childStores.getBootstrapState(DIRECTORY));
    return <span>{sessionCount}</span>;
  };

  const render = async () => {
    // SAFETY: SyncRuntime fixture narrowing — only `childStores` and
    // `currentDirectory` are reached by the hook under test; the loader/sdk
    // members keep their unexported module types and stay unused.
    const runtime = {
      childStores,
      currentDirectory: { get: () => DIRECTORY, subscribe: () => () => undefined },
      messageLoader: {},
      sdk: {},
      runtimeKey: 'test',
    } as SyncRuntimeContext;

    await act(async () => {
      root.render(
        <RuntimeProvider value={runtime}>
          <Probe />
        </RuntimeProvider>,
      );
    });
    mounted = true;
  };

  beforeEach(() => {
    const windowInstance = new Window();
    Object.assign(globalThis, {
      document: windowInstance.document,
      window: windowInstance,
      HTMLElement: windowInstance.HTMLElement,
      Element: windowInstance.Element,
      Node: windowInstance.Node,
      IS_REACT_ACT_ENVIRONMENT: true,
    });
    root = createRoot(document.createElement('div'));
    childStores = new ChildStoreManager();
    bootstrapped = [];
    duringRender = [];
    childStores.configure({
      onBootstrap: ({ directory }) => {
        bootstrapped.push(directory);
      },
    });
  });

  afterEach(async () => {
    if (mounted) {
      await act(async () => {
        root.unmount();
      });
      mounted = false;
    }
    childStores.disposeAll();
    // Leaked happy-dom globals make real store modules loaded by a later test
    // file in this process take the browser path and crash on bare `localStorage`.
    for (const key of ['document', 'window', 'HTMLElement', 'Element', 'Node', 'IS_REACT_ACT_ENVIRONMENT']) {
      Reflect.deleteProperty(globalThis, key);
    }
  });

  test('a render ensures the store without starting a bootstrap', async () => {
    await render();

    expect(duringRender[0]).toBeUndefined();
    expect(childStores.getChild(DIRECTORY)).toBeDefined();
  });

  test('the bootstrap is requested once the render commits', async () => {
    await render();

    expect(bootstrapped).toEqual([DIRECTORY]);
    expect(childStores.getBootstrapState(DIRECTORY)).toBe('complete');
  });

  test('a re-render neither restarts the bootstrap nor re-publishes its request', async () => {
    await render();
    await render();

    expect(bootstrapped).toEqual([DIRECTORY]);
    expect(childStores.getBootstrapState(DIRECTORY)).toBe('complete');
  });
});
