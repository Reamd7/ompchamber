/**
 * git 子进程执行封装：非交互（GIT_TERMINAL_PROMPT=0）、超时与 maxBuffer
 * 上限、可选 SSH key 身份注入（-c core.sshCommand），并把失败转成结构化
 * 结果而非异常，供技能目录的扫描与安装复用。
 */
import { execFile } from 'child_process';
import { promisify } from 'util';

/** promisify 后的 execFile，实际拉起 git 子进程。 */
const execFileAsync = promisify(execFile);

/** 单条 git 命令的默认超时：60 秒。 */
const DEFAULT_TIMEOUT_MS = 60_000;
/** 子进程 stdout/stderr 的默认 maxBuffer：4MB。 */
const DEFAULT_MAX_BUFFER = 4 * 1024 * 1024;

/** 按 stderr/message 文本特征判断是否认证或权限类错误，供把克隆失败归类为 authRequired。 */
export function looksLikeAuthError(message) {
  const text = String(message || '');
  return (
    /permission denied/i.test(text) ||
    /publickey/i.test(text) ||
    /could not read from remote repository/i.test(text) ||
    /authentication failed/i.test(text) ||
    /fatal: could not/i.test(text)
  );
}

/**
 * 执行一条 git 命令。成功返回 { ok: true, stdout, stderr }；失败不抛异常，
 * 返回 { ok: false, stdout, stderr, message, code, signal }。传入
 * identity.sshKey 时通过 -c core.sshCommand 指定密钥，并以 BatchMode=yes
 * 与 StrictHostKeyChecking=accept-new 避免任何交互式提示导致挂起。
 */
export async function runGit(args, options = {}) {
  const cwd = options.cwd;
  const timeoutMs = Number.isFinite(options.timeoutMs) ? options.timeoutMs : DEFAULT_TIMEOUT_MS;
  const maxBuffer = Number.isFinite(options.maxBuffer) ? options.maxBuffer : DEFAULT_MAX_BUFFER;

  const identity = options.identity || null;
  const normalizedArgs = Array.isArray(args) ? args.slice() : [];

  // Non-interactive git (avoid prompts / hangs)
  const env = {
    ...process.env,
    GIT_TERMINAL_PROMPT: '0',
  };

  if (identity?.sshKey) {
    const sshKeyPath = String(identity.sshKey).trim();
    if (sshKeyPath) {
      // Avoid interactive host key prompts; still safe against changed keys.
      const sshCommand = `ssh -i ${sshKeyPath} -o BatchMode=yes -o StrictHostKeyChecking=accept-new`;
      normalizedArgs.unshift(`core.sshCommand=${sshCommand}`);
      normalizedArgs.unshift('-c');
    }
  }

  try {
    const { stdout, stderr } = await execFileAsync('git', normalizedArgs, {
      cwd,
      env,
      windowsHide: true,
      timeout: timeoutMs,
      maxBuffer,
    });

    return { ok: true, stdout: stdout || '', stderr: stderr || '' };
  } catch (error) {
    const err = error;
    const stdout = typeof err?.stdout === 'string' ? err.stdout : '';
    const stderr = typeof err?.stderr === 'string' ? err.stderr : '';
    const message = err instanceof Error ? err.message : String(err);

    return {
      ok: false,
      stdout,
      stderr,
      message,
      code: typeof err?.code === 'number' ? err.code : null,
      signal: typeof err?.signal === 'string' ? err.signal : null,
    };
  }
}

/** 用 git --version 探测 git 是否可用；不可用返回 kind 为 gitUnavailable 的错误。 */
export async function assertGitAvailable() {
  const result = await runGit(['--version'], { timeoutMs: 5_000 });
  if (!result.ok) {
    return { ok: false, error: { kind: 'gitUnavailable', message: 'Git is not available in PATH' } };
  }
  return { ok: true };
}
