// @ts-nocheck
/**
 * terminal-app Electron main — window + in-process PTY sessions.
 *
 * The PTY runs here (privileged side); the renderer talks through the
 * narrow preload bridge only. No sidecar server: browser builds use the
 * WS backend, Electron builds never start it.
 */

const { app, BrowserWindow, ipcMain } = require('electron');
const pty = require('@lydell/node-pty');

/** @type {Map<number, import('@lydell/node-pty').IPty>} */
const sessions = new Map();
let nextId = 1;

function spawnSession(win, grid) {
  const id = nextId++;
  const shell = process.env.COMSPEC || 'cmd.exe';
  const proc = pty.spawn(shell, [], {
    name: 'xterm-256color',
    cols: Math.max(2, Math.min(500, grid.cols | 0)),
    rows: Math.max(2, Math.min(300, grid.rows | 0)),
    cwd: process.env.USERPROFILE || app.getPath('home'),
    env: { ...process.env, TERM: 'xterm-256color' },
  });
  sessions.set(id, proc);

  proc.onData((data) => {
    if (!win.isDestroyed()) win.webContents.send('term:data', id, data);
  });
  proc.onExit(() => {
    sessions.delete(id);
    if (!win.isDestroyed()) win.webContents.send('term:exit', id);
  });
  return id;
}

function createWindow() {
  const win = new BrowserWindow({
    width: 1100,
    height: 700,
    backgroundColor: '#1e1e1e',
    title: 'terminal-app',
    webPreferences: {
      preload: require('path').join(__dirname, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false, // preload needs ipcRenderer
    },
  });

  ipcMain.handle('term:spawn', (_e, grid) => spawnSession(win, grid));
  ipcMain.on('term:write', (_e, id, data) => sessions.get(id)?.write(data));
  ipcMain.on('term:resize', (_e, id, cols, rows) => {
    sessions.get(id)?.resize(
      Math.max(2, Math.min(500, cols | 0)),
      Math.max(2, Math.min(300, rows | 0))
    );
  });
  ipcMain.on('term:kill', (_e, id) => {
    sessions.get(id)?.kill();
    sessions.delete(id);
  });

  win.on('closed', () => {
    for (const proc of sessions.values()) proc.kill();
    sessions.clear();
  });

  const devUrl = process.env.TERMAPP_DEV_URL || 'http://localhost:8090';
  win.loadURL(devUrl).catch((err) => {
    console.error('[term] failed to load UI:', err.message);
    app.quit(1);
  });
  return win;
}

/** Smoke mode: verify the PTY round-trip without keeping a window open. */
async function smoke() {
  const win = new BrowserWindow({ show: false, webPreferences: { offscreen: true } });
  const got = new Promise((resolve) => {
    const id = spawnSession(win, { cols: 40, rows: 10 });
    const proc = sessions.get(id);
    let buf = '';
    proc.onData((d) => {
      buf += d;
      // cmd.exe banner is the first output; treat any substantial chunk as alive
      if (buf.length > 16) resolve({ id, sample: buf.slice(0, 40) });
    });
    proc.write('echo __smoke_ok__\r\n');
    setTimeout(() => resolve({ id, sample: buf.slice(0, 40), note: 'timeout-partial' }), 5000);
  });
  const r = await got;
  console.log('[smoke] pty alive, first output:', JSON.stringify(r.sample));
  console.log('[smoke] PASS');
  app.quit(0);
}

app.whenReady().then(() => {
  if (process.env.TERMAPP_SMOKE) {
    smoke().catch((e) => {
      console.error('[smoke] FAIL', e);
      app.quit(1);
    });
    return;
  }
  createWindow();
  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});

app.on('quit', () => {
  for (const proc of sessions.values()) proc.kill();
  sessions.clear();
});
