import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { Window } from 'happy-dom';
import React, { act, useLayoutEffect } from 'react';
import { createRoot, type Root } from 'react-dom/client';

import { useFilePreviewScrollPosition } from './useFilePreviewScrollPosition';

function Preview({ positionKey, element, onReady }: {
  positionKey: string | null;
  element: HTMLElement;
  onReady: (restore: () => void) => void;
}) {
  const { setScroller, restore } = useFilePreviewScrollPosition(positionKey);
  useLayoutEffect(() => {
    setScroller(element);
    return () => setScroller(null);
  }, [element, setScroller]);
  useLayoutEffect(() => onReady(restore), [onReady, restore]);
  return null;
}

describe('file preview scroll positions', () => {
  let windowInstance: Window;
  let root: Root;
  let scroller: HTMLDivElement;
  let content: HTMLDivElement;
  let height: number;
  let top: number;
  let left: number;
  let restore: () => void;
  let prefix: string;
  let sequence = 0;
  const onReady = (callback: () => void) => { restore = callback; };

  beforeEach(() => {
    windowInstance = new Window();
    Object.assign(globalThis, {
      window: windowInstance,
      document: windowInstance.document,
      HTMLElement: windowInstance.HTMLElement,
      Event: windowInstance.Event,
      MutationObserver: windowInstance.MutationObserver,
      ResizeObserver: windowInstance.ResizeObserver,
      IS_REACT_ACT_ENVIRONMENT: true,
    });
    const host = document.createElement('div');
    scroller = document.createElement('div');
    content = document.createElement('div');
    scroller.append(content);
    document.body.append(host, scroller);
    root = createRoot(host);
    height = 2000;
    top = 0;
    left = 0;
    prefix = `preview-test-${sequence++}`;
    Object.defineProperties(scroller, {
      scrollTop: {
        get: () => top,
        set: (value: number) => { top = Math.max(0, Math.min(value, height - 100)); },
      },
      scrollLeft: {
        get: () => left,
        set: (value: number) => { left = Math.max(0, Math.min(value, 500)); },
      },
    });
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    await windowInstance.happyDOM.close();
  });

  const render = async (key: string | null) => {
    await act(async () => {
      root.render(<Preview positionKey={key === null ? null : `${prefix}:${key}`} element={scroller} onReady={onReady} />);
    });
  };
  const scroll = (nextTop: number, nextLeft = 0) => {
    scroller.scrollTop = nextTop;
    scroller.scrollLeft = nextLeft;
    scroller.dispatchEvent(new Event('scroll'));
  };

  test('keeps positions independent across files, runtimes, modes and surfaces', async () => {
    const keys = ['runtime-a:file-a:code', 'runtime-a:file-b:code', 'runtime-b:file-a:code', 'runtime-a:file-a:markdown', 'runtime-a:file-a:code:fullscreen'];
    for (const [index, key] of keys.entries()) {
      await render(key);
      expect(top).toBe(0);
      scroll((index + 1) * 150, (index + 1) * 20);
    }
    for (const [index, key] of keys.entries()) {
      await render(key);
      expect(top).toBe((index + 1) * 150);
      expect(left).toBe((index + 1) * 20);
    }
  });

  test('ignores scroll collapse during loading and restores after a full view unmount', async () => {
    await render('file');
    scroll(900, 120);
    await render(null);
    scroll(0);
    await act(async () => root.render(null));
    await render('file');
    expect(top).toBe(900);
    expect(left).toBe(120);
  });

  test('waits for asynchronous code rendering without saving its clamped offset', async () => {
    await render('code');
    scroll(1200);
    await render(null);
    height = 200;
    await render('code');
    expect(top).toBe(100);
    scroll(100);
    height = 2000;
    restore();
    expect(top).toBe(1200);
    scroll(700);
    restore();
    expect(top).toBe(700);
    await render(null);
    await render('code');
    expect(top).toBe(700);
  });

  test('restores when lazy Markdown content mounts', async () => {
    await render('markdown');
    scroll(1000);
    await render(null);
    height = 100;
    await render('markdown');
    expect(top).toBe(0);
    height = 2000;
    content.append(document.createElement('p'));
    await windowInstance.happyDOM.waitUntilComplete();
    expect(top).toBe(1000);
  });

  test('stops pending restoration when the user starts scrolling', async () => {
    await render('markdown');
    scroll(1000);
    await render(null);
    height = 300;
    await render('markdown');
    scroller.dispatchEvent(new Event('wheel'));
    scroll(80);
    height = 2000;
    restore();
    expect(top).toBe(80);
    await render(null);
    await render('markdown');
    expect(top).toBe(80);
  });

  test('disconnects late content callbacks when leaving the file', async () => {
    await render('first');
    scroll(1000);
    await render(null);
    height = 200;
    await render('first');
    await render('second');
    scroll(50);
    height = 2000;
    content.append(document.createElement('p'));
    await windowInstance.happyDOM.waitUntilComplete();
    restore();
    expect(top).toBe(50);
  });
});
