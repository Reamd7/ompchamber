//! Port of `createSseBoundaryTracker` from `server/lib/opencode/proxy.js`:
//! tracks whether a raw upstream SSE byte stream currently sits between event
//! blocks (tail empty or ends with a blank line). Heartbeats and hub-merged
//! frames are only injected at boundaries so no upstream event is split.
//!
//! 中文说明：SSE 事件边界追踪器——维护上游字节流的尾部窗口，判断当前
//! 是否恰好停在两个事件块之间（尾部为空或以空行结尾）。心跳与 hub 合并
//! 帧只在边界处注入，保证不会截断上游的任何事件。

/// 尾部窗口保留的最大字符数；超出即丢弃更早内容，防止半截事件永久滞留。
pub(crate) const SSE_BOUNDARY_TAIL_MAX: usize = 4096;

/// 上游 SSE 流的边界状态：只保留最近 [`SSE_BOUNDARY_TAIL_MAX`] 个字符。
#[derive(Debug, Default)]
pub(crate) struct SseBoundaryTracker {
    /// 最近一段上游文本（`\r\n`/`\r` 已归一为 `\n`），仅用于判断边界。
    tail: String,
}

/// 边界追踪器的构造与查询。
impl SseBoundaryTracker {
    /// 创建尾部为空的追踪器（此时天然处于事件边界）。
    pub(crate) fn new() -> Self {
        Self {
            tail: String::new(),
        }
    }

    /// Feed the next upstream chunk; returns whether the stream is at an event
    /// boundary after it (mirrors `observe`).
    ///
    /// 中文说明：喂入新的上游 chunk——跨 chunk 边界的 UTF-8 按有损解码处理
    /// （只有 `\n` 字节影响判定），`\r\n` 与裸 `\r` 统一折叠成 `\n`，尾部
    /// 超限时截断到上限，最后返回喂入后是否处于事件边界。
    pub(crate) fn observe(&mut self, chunk: &[u8]) -> bool {
        // TextDecoder with stream:true — lossy across chunk edges is close
        // enough for boundary detection (only '\n' bytes matter).
        let text = String::from_utf8_lossy(chunk);
        if !text.is_empty() {
            let mut normalized = String::with_capacity(text.len());
            let mut chars = text.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\r' {
                    // "\r\n" collapses to a single "\n"; bare "\r" also becomes "\n".
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    normalized.push('\n');
                } else {
                    normalized.push(c);
                }
            }
            self.tail.push_str(&normalized);
            if self.tail.chars().count() > SSE_BOUNDARY_TAIL_MAX {
                let keep: String = self
                    .tail
                    .chars()
                    .rev()
                    .take(SSE_BOUNDARY_TAIL_MAX)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                self.tail = keep;
            }
        }
        self.is_at_boundary()
    }

    /// 当前是否处于事件边界：尾部为空，或以空行（`\n\n`）结尾。
    pub(crate) fn is_at_boundary(&self) -> bool {
        self.tail.is_empty() || self.tail.ends_with("\n\n")
    }
}

/// 边界追踪器单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证跨 chunk 拆分的事件流其边界判定与 JS 测试序列完全一致。
    #[test]
    fn tracks_boundaries_across_split_chunks() {
        // Exact sequence from the JS test
        // 'tracks whether a raw SSE stream is between event blocks'.
        let mut tracker = SseBoundaryTracker::new();

        assert!(tracker.is_at_boundary());
        assert!(!tracker.observe(b"id: evt-1\n"));
        assert!(!tracker.observe(b"data: {\"ok\""));
        assert!(!tracker.observe(b":true}\n"));
        assert!(tracker.observe(b"\n"));
        assert!(tracker.observe(b"data: next\r\n\r\n"));
    }

    /// 验证裸 `\r` 也按换行归一，`data: x\r\r` 之后即处于事件边界。
    #[test]
    fn normalizes_bare_carriage_returns_as_newlines() {
        let mut tracker = SseBoundaryTracker::new();
        assert!(tracker.observe(b"data: x\r\r"));
        assert!(tracker.is_at_boundary());
    }

    /// 验证尾部窗口有上限：超限截断后，未完成的事件不会永久阻塞边界判定。
    #[test]
    fn caps_the_tail_so_partial_events_cannot_stick_around_forever() {
        let mut tracker = SseBoundaryTracker::new();
        let filler = "a".repeat(SSE_BOUNDARY_TAIL_MAX + 100);
        assert!(!tracker.observe(filler.as_bytes()));
        assert!(tracker.tail.chars().count() <= SSE_BOUNDARY_TAIL_MAX);
        assert!(!tracker.is_at_boundary());
        assert!(tracker.observe(b"\n\n"));
        assert!(tracker.is_at_boundary());
    }
}
