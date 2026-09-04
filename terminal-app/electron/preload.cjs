// @ts-nocheck
/**
 * Preload bridge — the narrowest renderer-facing surface: one terminal
 * session factory plus its stream. Nothing else crosses the boundary.
 */
const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('termApp', {
  spawn: (grid) => ipcRenderer.invoke('term:spawn', grid),
  write: (session, data) => ipcRenderer.send('term:write', session, data),
  resize: (session, cols, rows) => ipcRenderer.send('term:resize', session, cols, rows),
  kill: (session) => ipcRenderer.send('term:kill', session),
  onData: (cb) => {
    const listener = (_e, session, chunk) => cb(session, chunk);
    ipcRenderer.on('term:data', listener);
  },
  onExit: (cb) => {
    const listener = (_e, session) => cb(session);
    ipcRenderer.on('term:exit', listener);
  },
});
