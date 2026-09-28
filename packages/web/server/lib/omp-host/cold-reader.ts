/**
 * 冷读（cold read）通道：只读访问 transcript 的 SessionManager 生命周期
 * 管理（docs/plan.md §7，阶段 3）。
 *
 * 冷 GET 路径（session info、消息历史、telemetry、结构化 entries、
 * transcript 上下文、entry 树）不允许留下可写的 SessionManager。SDK 的
 * writer 是惰性的（首次 append 才打开），因此"打开→读→关闭"的管理器在
 * 操作系统层面只读；残余成本是内存中的 entries 镜像，由
 * releaseRetainedEntries() 释放。所有冷读都经 withColdManager 执行：
 * finally 中关闭管理器并释放镜像，同时计数 opens/closes/releases，供
 * 测试证明没有任何冷 GET 泄漏管理器或其 entry 镜像。
 */
// ColdTranscriptReader (docs/plan.md §7, phase 3).
//
// Cold GET paths (session info, message history, telemetry, structured
// entries, transcript context, entry tree) must not leave a writable
// SessionManager behind. The SDK's writer is lazy (opened on first append),
// so an open→read→close manager is OS-level read-only; the residual cost is
// the in-memory `#entries` mirror, which `releaseRetainedEntries()` drops.
// Every cold read therefore runs through `withColdManager`, which:
//   - closes the manager in `finally` (flush+writer teardown),
//   - releases the retained entries mirror in `finally`,
//   - counts opens/closes/releases so tests can prove no cold GET leaks a
//     manager or its retained entry mirror.
import { SessionManager, loadSessionMessagesReadOnly } from '@oh-my-pi/pi-coding-agent';

/** 冷读者按结构化类型消费 SDK SessionManager 的表面接口。 */
/** Cold readers consume the SDK SessionManager surface structurally. */
export type ColdManager = Awaited<ReturnType<typeof SessionManager.open>>;
/** 冷读观测计数（plan §9.1：只记 O(1) 数字，绝不记内容）。 */
/** Observation counters (plan §9.1: O(1) numbers, never content). */
interface ColdReaderStats {
  /** 累计打开的管理器次数。 */
  opens: number;
  /** 累计成功关闭的管理器次数。 */
  closes: number;
  /** 累计释放 entries 镜像的次数。 */
  releases: number;
  /** close() 抛出异常的次数（管理器仍会被丢弃）。 */
  failedCloses: number;
}

/** 模块级计数器实例；经 coldReaderStats 读取、resetColdReaderStats 清零。 */
const stats: ColdReaderStats = {
  opens: 0,
  closes: 0,
  releases: 0,
  failedCloses: 0,
};

/** 读取当前计数快照（浅拷贝，调用方改不动内部状态）。 */
export const coldReaderStats = (): ColdReaderStats => ({ ...stats });

/** Test seam: reset counters between suites. */
/** 中文补充：测试接缝——在套件之间把四个计数器全部清零。 */
export const resetColdReaderStats = (): void => {
  stats.opens = 0;
  stats.closes = 0;
  stats.releases = 0;
  stats.failedCloses = 0;
};

/**
 * Open a cold manager, run `consume`, and guarantee release: close (flush +
 * writer teardown) and releaseRetainedEntries (drop the in-memory entry
 * mirror) both run in `finally` on every path, including consume throws
 * (plan §7.1). Returns consume's value.
 *
 * Order is contractual: the SDK documents releaseRetainedEntries as
 * "only call from session dispose, after the final close()" — it seals the
 * manager and drops pending writes, so close must run first.
 */
/**
 * 中文补充：打开冷管理器、执行 consume、并保证释放：close（flush +
 * writer 拆除）与 releaseRetainedEntries（丢弃内存 entry 镜像）在
 * finally 中对每条路径（含 consume 抛错）都执行（plan §7.1），返回
 * consume 的值。顺序是契约性的：SDK 文档要求 releaseRetainedEntries
 * 只能在最终 close() 之后调用（它会封存管理器并丢弃待写内容）。
 */
export async function withColdManager<T>(
  filePath: string,
  consume: (manager: ColdManager) => Promise<T> | T,
): Promise<T> {
  stats.opens += 1;
  const manager = await SessionManager.open(filePath);
  try {
    return await consume(manager);
  } finally {
    try {
      await manager.close();
      stats.closes += 1;
    } catch {
      stats.failedCloses += 1;
    }
    try {
      manager.releaseRetainedEntries();
      stats.releases += 1;
    } catch {
      // Release is best-effort; the manager is discarded regardless.
    }
  }
}

// Note: the SDK's `loadSessionMessagesReadOnly` (no manager at all) is the
// labeled full-materialization fallback for message-only consumers. The
// engine's message-history arm needs entries too (turn-state stampers and
// dividers), so it uses withColdManager; a message-only route should call
// the SDK loader directly and release its result promptly (plan §7.1).
// 中文补充：SDK 的 loadSessionMessagesReadOnly（完全不开管理器）是
// 面向纯消息消费者的"全量物化"回退路径。引擎的消息历史分支还需要
// entries（turn-state stampers 与分隔线），故走 withColdManager；纯
// 消息路由应直接调用 SDK 加载器并尽快释放其结果（plan §7.1）。
