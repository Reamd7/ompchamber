//! Port of `createSseBoundaryTracker` from `server/lib/opencode/proxy.js`:
//! tracks whether a raw upstream SSE byte stream currently sits between event
//! blocks (tail empty or ends with a blank line). Heartbeats and hub-merged
//! frames are only injected at boundaries so no upstream event is split.

pub(crate) const SSE_BOUNDARY_TAIL_MAX: usize = 4096;

#[derive(Debug, Default)]
pub(crate) struct SseBoundaryTracker {
    tail: String,
}

impl SseBoundaryTracker {
    pub(crate) fn new() -> Self {
        Self {
            tail: String::new(),
        }
    }

    /// Feed the next upstream chunk; returns whether the stream is at an event
    /// boundary after it (mirrors `observe`).
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

    pub(crate) fn is_at_boundary(&self) -> bool {
        self.tail.is_empty() || self.tail.ends_with("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn normalizes_bare_carriage_returns_as_newlines() {
        let mut tracker = SseBoundaryTracker::new();
        assert!(tracker.observe(b"data: x\r\r"));
        assert!(tracker.is_at_boundary());
    }

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
