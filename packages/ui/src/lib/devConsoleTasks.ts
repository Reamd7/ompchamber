// Dev-only: remove `console.createTask` before React DOM initializes.
//
// React 19.2 development builds wrap every component render and effect in a
// `console.createTask(...)` task (`fiber._debugTask.run`), which gives DevTools
// async stack attribution. Each task creation/invocation captures stack state,
// and with DevTools attached the cost grows by orders of magnitude: a chat
// switch into a large session measured an 8.2s main-thread task, of which the
// task wrapper itself was the dominant self-time entry. react-dom captures
// `console.createTask` once at module initialization, so clearing the method
// before react-dom loads makes it fall back to running callbacks directly.
//
// Production builds never execute the body (guarded by import.meta.env.DEV).
// To restore task attribution for a debugging session, set localStorage
// `oc-dev-console-tasks` to `'1'` and reload.
//
// This file must be imported FIRST in every UI entrypoint, before react-dom.

if (import.meta.env.DEV && typeof console !== 'undefined' && typeof window !== 'undefined') {
  try {
    const keepTasks = window.localStorage?.getItem('oc-dev-console-tasks') === '1';
    if (!keepTasks && 'createTask' in console) {
      Object.defineProperty(console, 'createTask', { value: undefined, configurable: true });
    }
  } catch {
    // localStorage can throw in restricted contexts; keep createTask then.
  }
}

export {};
