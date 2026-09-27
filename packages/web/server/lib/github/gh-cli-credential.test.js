/**
 * gh CLI 凭据读取（gh-cli-credential.js）单元测试套件。
 *
 * mock 掉 child_process.execFileSync，验证读取 token 时传入了
 * windowsHide（Windows 下不弹子进程窗口），以及 gh 不可用时返回 null
 * 且在缓存 TTL 内不再重复起子进程、清缓存后才重新探测。
 */
import { beforeEach, describe, expect, mock, test } from 'bun:test';

/** child_process.execFileSync 的 mock，默认返回空字符串。 */
const execFileSyncMock = mock(() => '');

mock.module('child_process', () => ({
  execFileSync: execFileSyncMock,
}));

// 在 mock.module 生效后动态加载被测模块。
const { clearGhCliTokenCache, getGhCliToken } = await import('./gh-cli-credential.js');

/** gh CLI 凭据的读取与缓存行为。 */
describe('gh CLI credential lookup', () => {
  beforeEach(() => {
    execFileSyncMock.mockReset();
    clearGhCliTokenCache();
  });

  test('hides the subprocess window on Windows', () => {
    execFileSyncMock.mockReturnValueOnce('token\n');

    expect(getGhCliToken()).toBe('token');
    expect(execFileSyncMock).toHaveBeenCalledWith('gh', ['auth', 'token'], {
      encoding: 'utf8',
      stdio: ['pipe', 'pipe', 'pipe'],
      timeout: 5000,
      windowsHide: true,
    });
  });

  test('caches unavailable gh CLI result until cache is cleared', () => {
    execFileSyncMock.mockImplementation(() => {
      throw new Error('gh unavailable');
    });

    expect(getGhCliToken()).toBeNull();
    expect(getGhCliToken()).toBeNull();
    expect(execFileSyncMock).toHaveBeenCalledTimes(1);

    clearGhCliTokenCache();

    expect(getGhCliToken()).toBeNull();
    expect(execFileSyncMock).toHaveBeenCalledTimes(2);
  });
});
