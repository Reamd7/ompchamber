// Per-machine relay-host claim. Every OMPChamber instance on a machine shares
// the same data dir and therefore the same relay signing key / serverId, so if
// two processes run a relay host at once they fight over the single host slot
// at the relay worker (each new connection closes the previous one with
// "4001: Control replaced") and paired devices land on whichever instance won
// last — often a dev/worktree instance running different code.
//
// The claim file (`relay-host.lock` in the shared data dir) makes the contest
// deterministic instead of a network race:
//   - an instance only starts its relay host when there is no LIVE claimant
//     (a dead claimant's stale file is ignored);
//   - explicit user intent (creating a pairing link) claims unconditionally —
//     the instance the user is interacting with must be the one devices reach;
//   - a running host that discovers another live process has claimed backs off
//     instead of reconnecting, which is what ends the replace/reconnect fight.
//
// This is a cooperative claim, not an OS lock: correctness does not depend on
// atomicity (the relay worker still enforces a single host); the claim only
// decides which process KEEPS retrying and which stands down.

/**
 * 【模块】relay host 单机占用声明（relay-host.lock）。
 *
 * 同一台机器上的所有 OMPChamber 实例共享同一数据目录，也就共享同一把 relay
 * 签名密钥 / serverId；若两个进程同时运行 relay host，它们会在 relay worker
 * 处争抢唯一的主机槽位（新连接以 "4001: Control replaced" 顶掉旧连接），
 * 已配对设备最终落到"最后获胜的那个实例"——常常是跑着不同代码的开发/worktree 实例。
 *
 * 锁文件（共享数据目录下的 `relay-host.lock`）把这场竞争从网络竞速变成确定性决策：
 *   - 仅当不存在 LIVE 声明者时实例才启动 relay host（已死声明者留下的过期文件被忽略）；
 *   - 显式用户意图（创建配对链接）无条件抢占——用户正在交互的实例必须是设备连到的那一个；
 *   - 运行中的 host 一旦发现其他活进程已声明就退避而不是重连，从而终结 replace/reconnect 打架。
 *
 * 这是协作式声明而非 OS 锁：正确性不依赖原子性（relay worker 仍强制单 host），
 * 声明只决定哪个进程"继续重试"、哪个进程"退避让位"。
 */
/**
 * 创建 relay host 锁实例。返回 tryClaim / forceClaim / holdsClaim /
 * liveClaimantPid / release 五个操作；所有操作都同步读写锁文件，
 * 不抛出异常（写失败仅告警并按成功处理，交由 relay worker 仲裁）。
 * @param {{
 *   lockFilePath: string,
 *   fs?: typeof import('node:fs'),
 *   process?: NodeJS.Process,
 *   logger?: Pick<Console, 'warn'>,
 * }} deps
 */
export const createRelayHostLock = ({ lockFilePath, fs, process: proc, logger = console }) => {
  const fsImpl = fs;
  const selfPid = proc.pid;

  /** 探测 pid 是否存活：signal 0 探测成功、或 EPERM（进程存在但属其他用户）都视为活；仅 ESRCH 视为已死。 */
  const isPidAlive = (pid) => {
    if (!Number.isInteger(pid) || pid <= 0) return false;
    try {
      proc.kill(pid, 0);
      return true;
    } catch (error) {
      // EPERM means the process exists but belongs to another user — treat as
      // alive; only ESRCH (no such process) means the claim is stale.
      return error?.code === 'EPERM';
    }
  };

  /** 读取锁文件声明并解析为 { pid }；文件缺失、JSON 损坏或 pid 非法一律视为无有效声明（返回 null）。 */
  const readClaim = () => {
    try {
      const raw = fsImpl.readFileSync(lockFilePath, 'utf8');
      const parsed = JSON.parse(raw);
      const pid = Number(parsed?.pid);
      return Number.isInteger(pid) && pid > 0 ? { pid } : null;
    } catch {
      // Missing file or unparsable content: no valid claim.
      return null;
    }
  };

  /**
   * 将本进程声明（pid + ISO 时间戳）写入锁文件。写入失败只告警仍返回 true：
   * 数据目录不可写不应拖垮 relay——回退到无锁时代的行为（照常启动 host，
   * 由 relay worker 仲裁）。
   */
  const writeClaim = () => {
    try {
      fsImpl.writeFileSync(lockFilePath, JSON.stringify({ pid: selfPid, claimedAt: new Date().toISOString() }));
      return true;
    } catch (error) {
      // An unwritable data dir must not take the relay down with it — fall back
      // to pre-lock behavior (start the host, let the relay worker arbitrate).
      logger.warn(`[Relay] could not write host claim file: ${error?.message ?? error}`);
      return true;
    }
  };

  /** 返回当前 LIVE 声明者的 pid；声明空闲或已过期（声明者已死）时返回 null。 */
  /** The pid of the current live claimant, or null when the claim is free/stale. */
  const liveClaimantPid = () => {
    const claim = readClaim();
    if (!claim) return null;
    return isPidAlive(claim.pid) ? claim.pid : null;
  };

  /** 条件抢占：仅当没有其他 LIVE 进程持有时才写入声明；重复声明自己等价于一次无害刷新。 */
  /** Claim unless another LIVE process already holds it. Re-claiming our own is a no-op refresh. */
  const tryClaim = () => {
    const holder = liveClaimantPid();
    if (holder !== null && holder !== selfPid) return false;
    return writeClaim();
  };

  /** 无条件抢占——显式用户意图（配对）优先于任何当前持有者。 */
  /** Unconditional claim — explicit user intent (pairing) overrides any holder. */
  const forceClaim = () => writeClaim();

  /** 当前进程是否仍是 LIVE 声明者。 */
  /** True while this process is the live claimant. */
  const holdsClaim = () => liveClaimantPid() === selfPid;

  /** 只释放属于自己的声明；绝不删除其他进程写入的锁文件。 */
  /** Release only our own claim; never delete another process's. */
  const release = () => {
    const claim = readClaim();
    if (!claim || claim.pid !== selfPid) return;
    try {
      fsImpl.unlinkSync(lockFilePath);
    } catch {
      // Already gone or unwritable — nothing to do.
    }
  };

  return { tryClaim, forceClaim, holdsClaim, liveClaimantPid, release };
};
