// domain-chrome tests — spec 09 §5.0-5.2 (extension host surfaces).
//
// Contracts under test:
//  - R-E1 RpcExtensionUIRequest mirror: set/clear semantics, payload shape,
//    undefined clears, placement passthrough.
//  - R-E2 no lease gating: chrome handlers write without any lease existing.
//  - R-E3 observable drops: factory payloads count, never render.
//  - snapshot authority: revision monotonic, last-writer-wins per key,
//    widget line cap.

import { describe, expect, test } from 'bun:test';
import { createDomainChrome, registerChromeDomainRoutes } from './domain-chrome.js';

const DIR = '/repo';
const SESSION = 'ses_1';

const setup = () => {
  const published = [];
  const chrome = createDomainChrome({
    publishFor: (directory, payload) => published.push({ directory, payload }),
    now: (() => {
      let tick = 0;
      return () => ++tick * 1000;
    })(),
  });
  return { chrome, published };
};

describe('createDomainChrome — widget table', () => {
  test('string[] set stores and publishes the RpcExtensionUIRequest shape', () => {
    const { chrome, published } = setup();
    chrome.setWidget(DIR, SESSION, 'zhipu', ['GLM Max', '16%'], 'aboveEditor');
    const snapshot = chrome.snapshot(DIR);
    expect(snapshot.widgets).toEqual([
      { key: 'zhipu', lines: ['GLM Max', '16%'], placement: 'aboveEditor', sessionId: SESSION, updatedAt: 1000 },
    ]);
    expect(published).toEqual([
      { directory: DIR, payload: { kind: 'widget', key: 'zhipu', lines: ['GLM Max', '16%'], placement: 'aboveEditor' } },
    ]);
    expect(snapshot.revision).toBe(1);
  });

  test('undefined clears an existing widget and publishes the clear', () => {
    const { chrome, published } = setup();
    chrome.setWidget(DIR, SESSION, 'zhipu', ['a']);
    chrome.setWidget(DIR, SESSION, 'zhipu', undefined);
    expect(chrome.snapshot(DIR).widgets).toEqual([]);
    expect(published[1]).toEqual({ directory: DIR, payload: { kind: 'widget', key: 'zhipu' } });
  });

  test('clearing an absent widget is a no-op (no event, no revision bump)', () => {
    const { chrome, published } = setup();
    chrome.setWidget(DIR, SESSION, 'ghost', undefined);
    expect(published).toEqual([]);
    expect(chrome.snapshot(DIR).revision).toBe(0);
  });

  test('empty array clears (widget with zero lines is not a widget)', () => {
    const { chrome } = setup();
    chrome.setWidget(DIR, SESSION, 'k', ['x']);
    chrome.setWidget(DIR, SESSION, 'k', []);
    expect(chrome.snapshot(DIR).widgets).toEqual([]);
  });

  test('last writer wins per key; rows cap at 10 (SDK widget cap)', () => {
    const { chrome } = setup();
    chrome.setWidget(DIR, 'ses_a', 'k', ['from-a']);
    chrome.setWidget(DIR, 'ses_b', 'k', ['from-b']);
    const eleven = Array.from({ length: 11 }, (_, i) => `row-${i}`);
    chrome.setWidget(DIR, 'ses_b', 'k', eleven);
    const snapshot = chrome.snapshot(DIR);
    expect(snapshot.widgets).toHaveLength(1);
    expect(snapshot.widgets[0].lines).toEqual(eleven.slice(0, 10));
    expect(snapshot.widgets[0].sessionId).toBe('ses_b');
  });

  test('invalid keys and non-string rows are ignored defensively', () => {
    const { chrome, published } = setup();
    chrome.setWidget(DIR, SESSION, '', ['x']);
    chrome.setWidget(DIR, SESSION, 'bad', ['ok', 42]);
    expect(published).toEqual([]);
    expect(chrome.snapshot(DIR).widgets).toEqual([]);
  });
});

describe('createDomainChrome — status table', () => {
  test('set/clear/publish mirror the widget semantics', () => {
    const { chrome, published } = setup();
    chrome.setStatus(DIR, SESSION, 'tps', '38 tok/s');
    expect(chrome.snapshot(DIR).status).toEqual([
      { key: 'tps', text: '38 tok/s', sessionId: SESSION, updatedAt: 1000 },
    ]);
    chrome.setStatus(DIR, SESSION, 'tps', undefined);
    expect(chrome.snapshot(DIR).status).toEqual([]);
    expect(published).toEqual([
      { directory: DIR, payload: { kind: 'status', key: 'tps', text: '38 tok/s' } },
      { directory: DIR, payload: { kind: 'status', key: 'tps' } },
    ]);
  });

  test('empty string clears (statusText: "" is the RPC clear form)', () => {
    const { chrome } = setup();
    chrome.setStatus(DIR, SESSION, 'tps', 'x');
    chrome.setStatus(DIR, SESSION, 'tps', '');
    expect(chrome.snapshot(DIR).status).toEqual([]);
  });
});

describe('createDomainChrome — bridge handlers (R-E2/R-E3)', () => {
  test('bridge handlers write without any lease (passive surface)', () => {
    const { chrome } = setup();
    const handlers = chrome.bridgeHandlersFor(DIR, SESSION);
    handlers.setWidget('zhipu', ['line'], { placement: 'aboveEditor' });
    handlers.setStatus('tps', 'ok');
    expect(chrome.snapshot(DIR).widgets).toHaveLength(1);
    expect(chrome.snapshot(DIR).status).toHaveLength(1);
  });

  test('component-factory payloads count as drops and never store', () => {
    const { chrome } = setup();
    const handlers = chrome.bridgeHandlersFor(DIR, SESSION);
    handlers.setWidget('rich', () => ({}), { placement: 'aboveEditor' });
    handlers.noteDropped('setFooter.factory');
    handlers.noteDropped('setFooter.factory');
    const snapshot = chrome.snapshot(DIR);
    expect(snapshot.widgets).toEqual([]);
    expect(snapshot.dropped).toEqual({ 'setWidget.factory': 1, 'setFooter.factory': 2 });
  });

  test('non-string status payloads count as drops', () => {
    const { chrome } = setup();
    chrome.bridgeHandlersFor(DIR, SESSION).setStatus('k', { complex: true });
    expect(chrome.snapshot(DIR).dropped).toEqual({ 'setStatus.invalid': 1 });
    expect(chrome.snapshot(DIR).status).toEqual([]);
  });
});

describe('registerChromeDomainRoutes', () => {
  const mount = (chrome, features) => {
    const routes = new Map();
    const route = (method, path, handler) => routes.set(`${method} ${path}`, handler);
    registerChromeDomainRoutes(route, { chrome, features });
    return routes;
  };

  test('missing directory answers 400', async () => {
    const { chrome } = setup();
    const routes = mount(chrome, { 'extensionChrome.v1': true });
    const response = await routes.get('GET /omp/chrome')(new Request('http://x/omp/chrome'));
    expect(response.status).toBe(400);
  });

  test('feature off answers 501 with the key in the error', async () => {
    const { chrome } = setup();
    const routes = mount(chrome, { 'extensionChrome.v1': false });
    const response = await routes.get('GET /omp/chrome')(
      new Request('http://x/omp/chrome?directory=%2Frepo'),
    );
    expect(response.status).toBe(501);
    expect(await response.json()).toEqual({ error: 'extensionChrome.v1-unavailable' });
  });

  test('feature on returns the snapshot', async () => {
    const { chrome } = setup();
    chrome.setWidget(DIR, SESSION, 'zhipu', ['line'], 'aboveEditor');
    const routes = mount(chrome, { 'extensionChrome.v1': true });
    const response = await routes.get('GET /omp/chrome')(
      new Request(`http://x/omp/chrome?directory=${encodeURIComponent(DIR)}`),
    );
    expect(response.status).toBe(200);
    const body = await response.json();
    expect(body.widgets).toHaveLength(1);
    expect(body.widgets[0].key).toBe('zhipu');
  });

  test('unknown directory snapshots as empty (revision 0), not an error', async () => {
    const { chrome } = setup();
    const routes = mount(chrome, { 'extensionChrome.v1': true });
    const response = await routes.get('GET /omp/chrome')(
      new Request('http://x/omp/chrome?directory=%2Felsewhere'),
    );
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ revision: 0, widgets: [], status: [], dropped: {} });
  });
});

describe('dialog bridge delegation (domain-dialogs integration)', () => {
  test('bridge chrome members delegate to the table; absent chrome stays no-op', async () => {
    const { UiLeaseTable, PendingDialogRegistry, createDialogBridge } = await import('./domain-dialogs.js');
    const leases = new UiLeaseTable();
    const registry = new PendingDialogRegistry({ bus: { emit() {} } });

    const { chrome } = setup();
    const bridge = createDialogBridge({
      leases,
      registry,
      directory: DIR,
      sessionId: SESSION,
      chrome: chrome.bridgeHandlersFor(DIR, SESSION),
    });
    bridge.setWidget('zhipu', ['line'], { placement: 'aboveEditor' });
    bridge.setStatus('tps', 'ok');
    bridge.setFooter(() => {});
    expect(chrome.snapshot(DIR).widgets).toHaveLength(1);
    expect(chrome.snapshot(DIR).status).toHaveLength(1);
    expect(chrome.snapshot(DIR).dropped).toEqual({ 'setFooter.factory': 1 });

    const bare = createDialogBridge({ leases, registry, directory: DIR, sessionId: SESSION });
    expect(() => {
      bare.setWidget('k', ['x']);
      bare.setStatus('k', 'y');
      bare.setTitle('t');
    }).not.toThrow();
    expect(chrome.snapshot(DIR).widgets).toHaveLength(1);
  });
});
