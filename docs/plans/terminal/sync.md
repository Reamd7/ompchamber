# 终端同步总体规划（v4：驱动者 + 缩放联动协商）

> 核心洞察：**缩放改变有效视口**。fontSize 变了 → cellWidth 变了 → 你的屏幕能容纳的列数变了 → 上报给 server 的 viewport 变了 → min-size/驱动者协商跟着变。

## 有效视口模型

```
有效列数 = floor(containerWidth / cellWidth)
有效行数 = floor(containerHeight / cellHeight)

cellWidth/cellHeight 由 fontSize 和字体度量决定（用户缩放改变它们）

同一个 1200px 宽的屏幕：
  fontSize 10px → cellWidth 6px  → 200 列 → 报 "cols=200"
  fontSize 14px → cellWidth 8px  → 150 列 → 报 "cols=150"
  fontSize 20px → cellWidth 12px → 100 列 → 报 "cols=100" → 可能拉小 PTY
```

**上报给 server 的不是物理像素，是有效列×行**。

## 联动流程

```
用户 pinch 放大
  → 瞬时 CSS scale（视觉预览，网格不变）
  → 松手 snap → 改 fontSize 设置
  → ghostty-web 重新计算 cellWidth
  → TerminalViewport fit() 算出新的有效 cols×rows
  → onResize 回调 → 发 WS viewport 帧（新 cols/rows）
  → server recomputeGrid / 驱动者跟随
  → PTY 可能 resize → 所有端收到 resized/driverChanged
  → 各端跟随新网格（窄屏自动 CSS scale）
```

## 状态机（与 v3 相同，但输入是有效视口）

```
IDLE: PTY = min(所有端的有效列) × min(有效行)
DRIVEN: PTY = 驱动者的有效列 × 有效行
```

## 双层缩放

```
driverScale = min(1, 我的容器宽 / (驱动者网格列数 × 我的cellWidth))
  → 用我自己的 cellWidth 算，不是固定像素比
  → 小字号设备 → cellWidth 小 → driverScale 更接近 1（不太需要缩）
  → 大字号设备 → cellWidth 大 → driverScale 更小（需要缩更多）

userScale = 用户 pinch snap 后的 fontSize 变化
  → 改变 cellWidth → 改变有效视口 → 触发协商
  → 不再是纯视觉叠加，而是参与网格尺寸决定

最终显示 = driverScale (自动) × 瞬时 pinch preview (手势中) → snap 后融为一体
```

## 实施计划

| 步骤 | 内容 | 规模 |
|---|---|---|
| **服务端** | | |
| 1 | viewportDriver 状态 + claim/release 帧 + driverChanged 广播 | ~50 行 |
| 2 | DRIVEN 模式下 attach 不 recomputeGrid | ~10 行 |
| 3 | 驱动者断线自动释放 | ~5 行 |
| **客户端协议** | | |
| 4 | claim/release 导出 + driverChanged 处理 | ~30 行 |
| **渲染层** | | |
| 5 | viewport 上报改为有效列×行（fit 结果） | 确认已有 |
| 6 | TerminalViewport 双层缩放（driverScale 用自己 cellWidth 算） | ~60 行 |
| 7 | resizeGrid 区分驱动者/跟随者模式 | ~15 行 |
| **UI** | | |
| 8 | 接管/释放按钮 + 状态指示 | ~40 行 |
| 9 | pinch-to-zoom → snap → fontSize → viewport 更新 | ~50 行 |
| 10 | i18n | ~22 行 |

总计 ~280 行
