> 状态（2026-09-14 复核）：**已解决**。`getMessagesPage` 的冷读路径已改为窗口化读取
> （`cold-transcript-page.ts` 的 `readTranscriptMessagePage`，engine.ts:1692 起接入），
> 不再每次请求 `SessionManager.open` 全量解析后切页。`SessionManager.open` 仅保留在
> live 会话、子会话解析和 `getSession` 全量加载路径。详见 `plan.md` 阶段 3/§7.2。

---

# 原始记录（存档）

搁置一个问题:
  SessionManager 就接受当前的配置，需要全量读取整个会话。（可能之后的优化可以做到每个会话单独按需加载）


1. getMessagesPage 每次被调用都走完整流程：找文件 → SessionManager.open 全量解析 → 拼完整消息列表 → 最后才按 limit/before 切一页 → 用完全部扔掉(engine.ts:1008-1065)。下次请求来，从头再来。

这个性能极差。
