# TODO

omp agent 设置面重构(2026-08-22/23)已全部落地并推送;以下是剩余任务。详细进度与验收证据见 `docs/omp-parity/PROGRESS-2026-08-21.md` 与 `docs/omp-parity/99-ACCEPTANCE-EVIDENCE.md`(§6.17)。

## Agent 域直接后续(本轮解锁/遗留)

- [x] **定时任务 persona 字段接线**(2026-08-23 完成):任务编辑器的 agent 字段在 omp 运行时下换为 persona 选择器(`/api/omp/personas`,默认「标准」,降级态禁用),默认不再灌旧级联 agent;保存走原 `agent` 参数,引擎已按 persona 解析。行为测试 `ScheduledTaskEditorDialog.persona.test.tsx` 3/3。
- [ ] **AgentsPage 视觉项在 VS Code / Mobile 端实测一次**:discover/reveal/文档链接/web 端已真机验证,三端共享代码但未实跑(属 PROGRESS 第二档「三端一次都没实跑」)。
- [ ] **手动改 `.omp/agents/*.md` 后的 UI 感知**:服务端有 `POST /api/omp/agent-definitions/refresh`(外部编辑兜底),UI 侧暂无入口;可在 agents 页加刷新或挂 file-watch,低优先。

## 观察门后删除列车(≥2026-09-03,PROGRESS 第四档)

过门后批次 6,一次清掉旧面:

- [ ] 权限盾牌按钮、旧权限卡片链、question 卡片链
- [ ] `AgentPermissionsEditor`(legacy agents 表单随之下线)
- [ ] shell `!` 通道、wire 死事件消费链、`/command` 空端点
- [ ] plan 实验残迹文件、BehaviorPage 旧路径、share 复核
- [ ] wire `GET /agent` 的 build/plan 壳(07 章删除;omp 模式下 UI 已不消费)

## 批次 7 尾款(可随时做)

- [ ] E02:StatusRow 扩展段消费面(host 侧已就绪)
- [ ] E03:composer 预填(`setEditorText`)
- [ ] E04:`title` / `open_url`

## P2 未排期(PROGRESS 第四档 18 项,按优先级开)

- [ ] Agent Hub、parked 会话复活、尾读、subagent HUD、artifacts 浏览
- [ ] vibe / loop / prewalk 面板 / btw / tan、goal 自主续跑(前置测试先行)
- [ ] ssh / vault / security(前置=威胁评审)
- [ ] 消息撤回回收(message.part.removed)、逐轮完整渲染(ttsr/thinking)
- [ ] usage / context 对齐、多客户端队列一致(等上游 SDK 扩展)

## 待真机顺手验(PROGRESS 第二档)

- [ ] plan 评审弹层(四选一)、goal 指示器(需真实 goal)、模型降级徽标、retry 提示
- [ ] local:// 链接点开、subagent 运行行、ask 答案卡、托盘/系统通知(Electron)
- [ ] 桌面端 capabilities 500 排查(桌面 runtime 下 `/api/omp/capabilities`)
