import fs from 'fs';
import os from 'os';
import path from 'path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const legacyDir = () => path.join(os.homedir(), '.config', 'openchamber');
const currentDir = () => path.join(os.homedir(), '.config', 'ompchamber');

const loadMigration = async () => {
  vi.resetModules();
  const module = await import('./app-data-migration.js');
  return module.migrateLegacyAppDataDir;
};

const writeSettings = (dir, value) => {
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'settings.json'), JSON.stringify(value));
};

const readSettings = (dir) =>
  JSON.parse(fs.readFileSync(path.join(dir, 'settings.json'), 'utf8'));

describe('migrateLegacyAppDataDir', () => {
  let tempHome;
  let previousHome;
  let previousUserProfile;
  let previousDataDir;

  beforeEach(() => {
    tempHome = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-migration-test-'));
    previousHome = process.env.HOME;
    previousUserProfile = process.env.USERPROFILE;
    previousDataDir = process.env.OPENCHAMBER_DATA_DIR;
    delete process.env.OPENCHAMBER_DATA_DIR;
    process.env.HOME = tempHome;
    process.env.USERPROFILE = tempHome;
  });

  afterEach(() => {
    if (previousHome === undefined) delete process.env.HOME;
    else process.env.HOME = previousHome;
    if (previousUserProfile === undefined) delete process.env.USERPROFILE;
    else process.env.USERPROFILE = previousUserProfile;
    if (previousDataDir === undefined) delete process.env.OPENCHAMBER_DATA_DIR;
    else process.env.OPENCHAMBER_DATA_DIR = previousDataDir;
    fs.rmSync(tempHome, { recursive: true, force: true });
  });

  it('moves the legacy directory wholesale so existing data keeps working', async () => {
    writeSettings(legacyDir(), { uiPassword: 'secret' });
    const migrate = await loadMigration();

    migrate();

    expect(fs.existsSync(legacyDir())).toBe(false);
    expect(readSettings(currentDir())).toEqual({ uiPassword: 'secret' });
  });

  it('creates nothing when the legacy directory is absent', async () => {
    const migrate = await loadMigration();

    migrate();

    expect(fs.existsSync(legacyDir())).toBe(false);
    expect(fs.existsSync(currentDir())).toBe(false);
  });

  it('keeps the current directory when both exist', async () => {
    writeSettings(legacyDir(), { source: 'legacy' });
    writeSettings(currentDir(), { source: 'current' });
    const migrate = await loadMigration();

    migrate();

    expect(readSettings(currentDir())).toEqual({ source: 'current' });
    expect(readSettings(legacyDir())).toEqual({ source: 'legacy' });
  });

  it('never touches the home layout when OPENCHAMBER_DATA_DIR is set', async () => {
    writeSettings(legacyDir(), { uiPassword: 'secret' });
    process.env.OPENCHAMBER_DATA_DIR = path.join(tempHome, 'isolated-data');
    const migrate = await loadMigration();

    migrate();

    expect(fs.existsSync(legacyDir())).toBe(true);
    expect(fs.existsSync(currentDir())).toBe(false);
  });

  it('runs at most once per process even if the legacy directory reappears', async () => {
    writeSettings(legacyDir(), { uiPassword: 'secret' });
    const migrate = await loadMigration();

    migrate();
    writeSettings(legacyDir(), { uiPassword: 'recreated' });

    migrate();

    // The second call is a no-op: the recreated legacy directory stays put.
    expect(readSettings(legacyDir())).toEqual({ uiPassword: 'recreated' });
    expect(readSettings(currentDir())).toEqual({ uiPassword: 'secret' });
  });
});
