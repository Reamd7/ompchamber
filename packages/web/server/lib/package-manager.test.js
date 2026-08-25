import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// Mock child_process to prevent real spawnSync calls that would hang in tests
vi.mock('node:child_process', () => ({
  spawn: vi.fn(),
  spawnSync: vi.fn(() => ({ status: 0, stdout: '/usr/local/bin', stderr: '' })),
}));

const {
  checkForUpdates,
  detectPackageManager,
  executeUpdate,
  getCurrentVersion,
} = await import('./package-manager.js');

/** Helper: create a fetch mock that routes by URL pattern */
function createFetchMock() {
  const handlers = new Map();

  const mock = vi.fn((url, options) => {
    const urlStr = typeof url === 'string' ? url : url.toString();

    for (const [pattern, response] of handlers) {
      if (urlStr.includes(pattern)) {
        return Promise.resolve(response);
      }
    }

    return Promise.reject(new Error(`Unexpected fetch call: ${urlStr}`));
  });

  mock.when = (pattern, response) => {
    handlers.set(pattern, response);
    return mock;
  };

  return mock;
}

describe('checkForUpdates', () => {
  let fetchMock;
  let originalFetch;

  beforeEach(() => {
    fetchMock = createFetchMock();
    originalFetch = globalThis.fetch;
    globalThis.fetch = fetchMock;
  });

  afterEach(() => {
    globalThis.fetch = originalFetch;
    delete process.env.OMPCHAMBER_UPDATE_API_URL;
  });

  const latestRelease = (tag) => fetchMock.when('api.github.com/repos/Reamd7/openchamber/releases/latest', {
    ok: true,
    json: async () => ({ tag_name: tag }),
  });

  it('returns available=true when the latest GitHub release is newer', async () => {
    latestRelease('v1.10.0');
    fetchMock.when('raw.githubusercontent.com', {
      ok: true,
      text: async () => '## [1.10.0] - 2026-05-01\n\n- Great new feature',
    });

    const result = await checkForUpdates({ currentVersion: '1.9.10' });

    expect(result.available).toBe(true);
    expect(result.version).toBe('1.10.0');
    expect(result.currentVersion).toBe('1.9.10');
  });

  it('returns available=false when the latest release matches the current version', async () => {
    latestRelease('v1.9.10');

    const result = await checkForUpdates({ currentVersion: '1.9.10' });

    expect(result.available).toBe(false);
  });

  it('returns available=false when the latest release tag is an older prerelease', async () => {
    latestRelease('v1.10.0-beta.1');

    const result = await checkForUpdates({ currentVersion: '1.10.0' });

    expect(result.available).toBe(false);
  });

  it('never contacts the hosted update API when OMPCHAMBER_UPDATE_API_URL is unset', async () => {
    latestRelease('v1.10.0');
    fetchMock.when('raw.githubusercontent.com', {
      ok: true,
      text: async () => '## [1.10.0] - 2026-05-01\n\n- Great new feature',
    });

    const result = await checkForUpdates({
      appType: 'desktop-electron',
      currentVersion: '1.9.10',
      installId: '4f4dfead-9688-4c4f-97d7-4607fbbfc3ab',
      platform: 'windows',
      arch: 'arm64',
    });

    expect(result.available).toBe(true);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    const requestedUrls = fetchMock.mock.calls.map((call) => String(call[0]));
    expect(requestedUrls.some((url) => url.includes('api.openchamber.dev'))).toBe(false);
  });

  it('resolves an Android APK asset when an enabled update API returns an AAB', async () => {
    process.env.OMPCHAMBER_UPDATE_API_URL = 'https://api.openchamber.dev/v1/update/check';
    vi.resetModules();
    const { checkForUpdates: check } = await import('./package-manager.js');
    fetchMock
      .when('api.openchamber.dev', {
        ok: true,
        json: async () => ({
          latestVersion: '1.10.0',
          updateAvailable: true,
          downloadUrl: 'https://github.com/openchamber/openchamber/releases/download/v1.10.0/OpenChamber-1.10.0-42-android.aab',
        }),
      })
      .when('api.github.com/repos/Reamd7/openchamber/releases/tags/v1.10.0', {
        ok: true,
        json: async () => ({
          assets: [
            {
              name: 'OpenChamber-1.10.0-42-android.aab',
              browser_download_url: 'https://downloads.example/OpenChamber-1.10.0-42-android.aab',
            },
            {
              name: 'OpenChamber-1.10.0-42-android.apk',
              browser_download_url: 'https://downloads.example/OpenChamber-1.10.0-42-android.apk',
            },
          ],
        }),
      });

    const result = await check({
      appType: 'mobile-capacitor',
      platform: 'android',
      currentVersion: '1.9.10',
    });

    expect(result.downloadUrl).toBe('https://downloads.example/OpenChamber-1.10.0-42-android.apk');
  });

  it('keeps a direct Android APK URL from an enabled update API', async () => {
    process.env.OMPCHAMBER_UPDATE_API_URL = 'https://api.openchamber.dev/v1/update/check';
    vi.resetModules();
    const { checkForUpdates: check } = await import('./package-manager.js');
    const apkUrl = 'https://github.com/openchamber/openchamber/releases/download/v1.10.0/OpenChamber-1.10.0-42-android.apk';
    fetchMock.when('api.openchamber.dev', {
      ok: true,
      json: async () => ({
        latestVersion: '1.10.0',
        updateAvailable: true,
        downloadUrl: apkUrl,
      }),
    });

    const result = await check({
      appType: 'mobile-capacitor',
      platform: 'android',
      currentVersion: '1.9.10',
    });

    expect(result.downloadUrl).toBe(apkUrl);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('returns available=false when the GitHub releases API is unreachable', async () => {
    fetchMock.when('api.github.com', Promise.reject(new Error('Network error')));

    const result = await checkForUpdates({ currentVersion: '1.9.10' });

    expect(result.available).toBe(false);
  });

  it('returns available=false when the GitHub releases API responds non-ok', async () => {
    fetchMock.when('api.github.com', {
      ok: false,
      status: 500,
      json: async () => ({}),
    });

    const result = await checkForUpdates({ currentVersion: '1.9.10' });

    expect(result.available).toBe(false);
  });
});

describe('getCurrentVersion', () => {
  it('is exported for the CLI update command', () => {
    expect(typeof getCurrentVersion).toBe('function');
    expect(getCurrentVersion()).toMatch(/^\d+\.\d+\.\d+|unknown$/);
  });
});

describe('CLI update exports', () => {
  it('exports package-manager helpers used by the update command', () => {
    expect(typeof detectPackageManager).toBe('function');
    expect(typeof executeUpdate).toBe('function');
  });
});
