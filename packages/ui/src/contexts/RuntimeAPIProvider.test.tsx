/**
 * RuntimeAPIProvider content-cache owner lifecycle.
 *
 * bug.logs (dev console): "Maximum update depth exceeded", attributed to
 * RuntimeAPIProvider. The effect that adopts a content-cache owner also listed
 * the owner's own `files` as a dependency — and an owner's `files` is its
 * wrapper, never the `apis.files` it was built from. Every adoption therefore
 * changed a dependency, so the effect re-ran, disposed the owner it had just
 * adopted, adopted the next one, and never settled.
 *
 * Contracts:
 *  - re-renders reuse the owner, so consumers keep one `files` identity
 *  - a new `apis.files` identity replaces the owner once, disposing the old one
 *  - unmounting disposes the owner consumers were reading through
 *  - Strict Mode's dispose+remount leaves a live owner
 *
 * Rendered via happy-dom + createRoot/act (Bun provides no DOM).
 */
import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { Window } from 'happy-dom';
import React, { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';

import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import type { FilesAPI, RuntimeAPIs } from '@/lib/api/types';
import { RuntimeAPIProvider } from './RuntimeAPIProvider';

const DISPOSED_MESSAGE = 'File cache owner disposed';

/**
 * Only `readFile`/`statFile` carry behavior; the cache owner wraps those and
 * spreads the rest of the API through untouched.
 */
const createFiles = (tag: string): FilesAPI => ({
  listDirectory: async (path: string) => ({ directory: path, entries: [] }),
  search: async () => [],
  createDirectory: async (path: string) => ({ success: true, path }),
  statFile: async (path: string) => ({ path, isFile: true, size: tag.length, mtimeMs: 1 }),
  readFile: async (path: string) => ({ content: `${tag}:${path}`, path }),
});

// SAFETY: RuntimeAPIs fixture narrowing — the provider reads `apis.files` and
// spreads every other member into the context untouched, so the omitted APIs
// never participate.
const runtimeApis = (files: FilesAPI): RuntimeAPIs => ({ files } as RuntimeAPIs);

type Handle = { seen: FilesAPI[]; mounted: boolean };

const Probe = ({ handle }: { handle: Handle }) => {
  handle.seen.push(useRuntimeAPIs().files);
  return null;
};

describe('RuntimeAPIProvider content-cache owner', () => {
  let root: Root;
  let handle: Handle;

  const render = async (apis: RuntimeAPIs, strict = false) => {
    const tree = (
      <RuntimeAPIProvider apis={apis}>
        <Probe handle={handle} />
      </RuntimeAPIProvider>
    );
    await act(async () => {
      root.render(strict ? <React.StrictMode>{tree}</React.StrictMode> : tree);
    });
    handle.mounted = true;
  };

  const unmount = async () => {
    await act(async () => {
      root.unmount();
    });
    handle.mounted = false;
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
    handle = { seen: [], mounted: false };
  });

  afterEach(async () => {
    if (handle.mounted) await unmount();
    // Every happy-dom global assigned in beforeEach must go: a leaked `window`
    // makes real store modules loaded by a later test file in this process take
    // the browser path and crash on bare `localStorage`.
    for (const key of ['document', 'window', 'HTMLElement', 'Element', 'Node', 'IS_REACT_ACT_ENVIRONMENT']) {
      Reflect.deleteProperty(globalThis, key);
    }
  });

  test('re-rendering the same files API reuses one owner', async () => {
    const apis = runtimeApis(createFiles('a'));
    await render(apis);
    await render(apis);

    expect(new Set(handle.seen).size).toBe(1);
    const files = handle.seen[0]!;
    expect(await files.readFile!('/a.txt')).toEqual({ content: 'a:/a.txt', path: '/a.txt' });
  });

  test('a new files API identity replaces the owner once and disposes the previous one', async () => {
    await render(runtimeApis(createFiles('first')));
    const firstFiles = handle.seen.at(-1)!;
    const readThroughFirstOwner = firstFiles.readFile!;
    expect(await readThroughFirstOwner('/a.txt')).toEqual({ content: 'first:/a.txt', path: '/a.txt' });

    await render(runtimeApis(createFiles('second')));
    const secondFiles = handle.seen.at(-1)!;
    expect(secondFiles).not.toBe(firstFiles);
    await expect(readThroughFirstOwner('/a.txt')).rejects.toThrow(DISPOSED_MESSAGE);
    expect(await secondFiles.readFile!('/a.txt')).toEqual({ content: 'second:/a.txt', path: '/a.txt' });
  });

  test('unmounting disposes the owner consumers were reading through', async () => {
    await render(runtimeApis(createFiles('only')));
    const readThroughOwner = handle.seen.at(-1)!.readFile!;
    await unmount();

    await expect(readThroughOwner('/a.txt')).rejects.toThrow(DISPOSED_MESSAGE);
  });

  test("Strict Mode's dispose+remount leaves a live owner", async () => {
    await render(runtimeApis(createFiles('strict')), true);
    const live = handle.seen.at(-1)!;

    expect(await live.readFile!('/a.txt')).toEqual({ content: 'strict:/a.txt', path: '/a.txt' });
  });
});
