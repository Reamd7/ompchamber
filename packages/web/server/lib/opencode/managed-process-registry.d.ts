/**
 * 托管进程注册表的类型声明。
 *
 * 托管 omp host 子进程启动时注册自身（pid / ownerPid / port）；父进程
 * 意外退出后，下一次启动会通过 reapOrphanedProcesses 清理孤儿进程，
 * 避免端口被残留进程占用。
 */
/**
 * 登记一个由当前进程管理的子进程。
 * @param entry - 子进程信息：pid 为主键，ownerPid 用于孤儿判定，
 *   port/binary/runtime 仅用于诊断日志。
 */
export function registerManagedProcess(entry: {
  /** 子进程 PID。 */
  pid?: number;
  /** 父进程（所有者）PID，用于孤儿判定。 */
  ownerPid?: number;
  /** 子进程监听端口（未知为 null）。 */
  port?: number | null;
  /** 启动的二进制路径（日志诊断用）。 */
  binary?: string | null;
  /** 运行时名称（如 bun/node）。 */
  runtime?: string;
}): Promise<void>;

/** 按子进程 PID 注销登记（进程退出或不再由本进程托管时调用）。 */
export function unregisterManagedProcess(pid?: number): Promise<void>;

/**
 * 清理 ownerPid 已不存在的孤儿托管进程。
 * @param options.log - 自定义日志输出函数（缺省用 console）。
 * @returns 检查（inspected）与实际清理（reaped）的进程数量。
 */
export function reapOrphanedProcesses(options?: {
  /** 可选日志函数，接收逐条清理消息。 */
  log?: (message: string) => void;
}): Promise<{ inspected: number; reaped: number }>;
