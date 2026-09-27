//! Port of the vendored `@ompchamber/terminal-server` `GridCore`
//! (`server/lib/terminal/vendor/ompchamber-terminal-server/core.mjs`),
//! re-hosted on the `vt100` crate instead of `@xterm/headless`.
//!
//! One `GridCore` per terminal session parses the same raw PTY stream the byte
//! feed ships and turns it into grid diff frames: `full` / `rows` / `cursor`,
//! with cells encoded as `[codepoint, fg24, bg24, flags, width]` (fixed shape,
//! shared contract with the UI). Colors resolve through the ghostty-vt aligned
//! palette (`palette.mjs`).
//!
//! Known gaps vs the xterm-headless host (vt100 does not track them): the
//! `flags` bits for strikethrough (8), invisible (32) and dim (128) are always
//! 0; row-wrap state is not part of the wire frame either way.
//!
//! 中文说明：每个终端会话持有一个 `GridCore`，解析与字节 feed 相同的
//! 原始 PTY 流并产出网格 diff 帧。核心是 `drain`：逐行做 FNV-1a 哈希，
//! 与上一帧的哈希基线比较——无基线或改动行数超过六成时发 `full` 全量帧，
//! 少量改动发 `rows` 增量帧，无改动发 `cursor` 帧。与 UI 共享的单元格
//! 编码为 `[codepoint, fg24, bg24, flags, width]` 定长五元组，颜色经
//! ghostty-vt 对齐调色板解析。

use serde_json::{Value, json};

/// Default foreground as packed 24-bit RGB (0xRRGGBB).
pub const DEFAULT_FG: u32 = (204 << 16) | (204 << 8) | 204;
/// Default background as packed 24-bit RGB.
pub const DEFAULT_BG: u32 = 0;

/// Palette + color resolution matching ghostty-vt's built-in theme
/// (Tomorrow-style base, 6×6×6 cube, 24-step grayscale).
fn palette_rgb(idx: u8) -> (u32, u32, u32) {
    match idx {
        0..=15 => {
            const BASE: [(u32, u32, u32); 16] = [
                (204, 204, 204),
                (204, 102, 102),
                (181, 189, 104),
                (222, 147, 95),
                (129, 162, 190),
                (178, 148, 187),
                (138, 190, 183),
                (204, 204, 204),
                (117, 117, 117),
                (241, 141, 133),
                (219, 200, 106),
                (233, 190, 126),
                (138, 178, 235),
                (213, 161, 216),
                (148, 216, 209),
                (255, 255, 255),
            ];
            BASE[idx as usize]
        }
        16..=231 => {
            let c = idx as u32 - 16;
            const STEPS: [u32; 6] = [0, 95, 135, 175, 215, 255];
            (
                STEPS[(c / 36) as usize],
                STEPS[((c % 36) / 6) as usize],
                STEPS[(c % 6) as usize],
            )
        }
        _ => {
            let g = 8 + (idx as u32 - 232) * 10;
            (g, g, g)
        }
    }
}

/// `paletteColor`: resolve an xterm palette index to packed 24-bit RGB.
pub fn palette_color(idx: u8) -> u32 {
    let (r, g, b) = palette_rgb(idx);
    (r << 16) | (g << 8) | b
}

/// 把 vt100 单元格颜色解析为打包 24-bit RGB：`Default` 用调用方给的
/// 缺省值，`Rgb` 直接打包，`Idx` 查调色板。
fn color_of(color: vt100::Color, default: u32) -> u32 {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
        vt100::Color::Idx(idx) => palette_color(idx),
    }
}

/// Per-cell FNV-1a mixing input, exactly the JS expression
/// `cp + (fg << 8) + (bg << 4) + flags + width` (wrapping u32).
fn cell_mix(cp: u32, fg: u32, bg: u32, flags: u32, width: u32) -> u32 {
    cp.wrapping_add(fg << 8)
        .wrapping_add(bg << 4)
        .wrapping_add(flags)
        .wrapping_add(width)
}

/// 单元格文本属性映射为 wire flags 位：bold=1、italic=2、underline=4、
/// inverse=16（与 JS host 的位布局一致；strikethrough/invisible/dim
/// 见模块说明，恒为 0）。
fn cell_flags(cell: &vt100::Cell) -> u32 {
    (u32::from(cell.bold()))
        | (u32::from(cell.italic()) << 1)
        | (u32::from(cell.underline()) << 2)
        | (u32::from(cell.inverse()) << 4)
}

/// 单元格显示宽度：宽字符 2、宽字符的续格 0、其余 1，与 JS host 的
/// width 语义一致。
fn cell_width(cell: &vt100::Cell) -> u32 {
    if cell.is_wide() {
        2
    } else if cell.is_wide_continuation() {
        0
    } else {
        1
    }
}

/// 行内超出分配长度处的空白单元格：空格 + 默认前景/背景、无属性、宽 1。
fn blank_cell() -> Value {
    json!([32, DEFAULT_FG, DEFAULT_BG, 0, 1])
}

/// Transport-free server-side grid parser (see module docs). Frame JSON shapes
/// match the vendored `GridCore` exactly.
pub struct GridCore {
    /// vt100 解析器，持有屏幕状态并消费原始 VT 字节。
    parser: vt100::Parser,
    /// 当前有效列数（创建/resize 时已按上限收敛）。
    cols: u16,
    /// 当前有效行数（创建/resize 时已按上限收敛）。
    rows: u16,
    /// 列数上限，new/resize 的钳制边界。
    max_cols: u16,
    /// 行数上限，new/resize 的钳制边界。
    max_rows: u16,
    /// 上一帧的逐行 FNV-1a 哈希基线；`None` 表示无基线，下一次 drain
    /// 必发全量帧。
    last_hashes: Option<Vec<u32>>,
}

/// 构造、喂流、resize 与三种 diff 帧的产出。
impl GridCore {
    /// 以 JS host 的固定上限（1000 列 × 500 行）构造。
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_limits(cols, rows, 1000, 500)
    }

    /// 以显式上限构造；尺寸先经 `clamp_dim` 收敛到 2..=max 再创建解析器。
    pub fn with_limits(cols: u16, rows: u16, max_cols: u16, max_rows: u16) -> Self {
        let cols = clamp_dim(cols, max_cols);
        let rows = clamp_dim(rows, max_rows);
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            cols,
            rows,
            max_cols,
            max_rows,
            last_hashes: None,
        }
    }

    /// 当前有效列数（与帧 JSON 中的 `cols` 一致）。
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// 当前有效行数（与帧 JSON 中的 `rows` 一致）。
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Feed VT bytes. The JS host defers the drain to a `setTimeout(0)` that
    /// coalesces bursts; this port leaves the drain decision to the caller
    /// (the session runtime defers it the same way).
    pub fn write(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    /// `resize`: clamped 2..=max; a real change resets the diff baseline so
    /// the next drain emits a full frame.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = clamp_dim(cols, self.max_cols);
        let rows = clamp_dim(rows, self.max_rows);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.parser.set_size(rows, cols);
        self.last_hashes = None;
    }

    /// 光标三元组 `[x, y, true]`：坐标钳到网格范围内，第三位是可见
    /// 标志（JS host 恒为 true）。
    fn cursor_msg(&self) -> Value {
        let screen = self.parser.screen();
        let (row, col) = screen.cursor_position();
        let x = col.min(self.cols.saturating_sub(1));
        let y = row.min(self.rows.saturating_sub(1));
        json!([x, y, true])
    }

    /// `fullFrame`: complete state; resets the diff baseline. Hosts use this
    /// to materialize snapshots for attaching clients.
    pub fn full_frame(&mut self) -> Value {
        self.last_hashes = None;
        self.drain()
    }

    /// `drain`: the next diff frame (`full` / `rows` / `cursor`).
    pub fn drain(&mut self) -> Value {
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        let screen = self.parser.screen();
        let mut hashes: Vec<u32> = Vec::with_capacity(rows);
        let prev = self.last_hashes.take();
        let full = prev.is_none();
        let mut payload_rows: Vec<(usize, Vec<Value>)> = Vec::new();

        for y in 0..rows {
            let mut row_cells: Vec<Value> = Vec::with_capacity(cols);
            let mut h: u32 = 2166136261;
            for x in 0..cols {
                match screen.cell(y as u16, x as u16) {
                    Some(cell) => {
                        let cp = cell
                            .contents()
                            .chars()
                            .next()
                            .map(|c| c as u32)
                            .unwrap_or(32);
                        let fg = color_of(cell.fgcolor(), DEFAULT_FG);
                        let bg = color_of(cell.bgcolor(), DEFAULT_BG);
                        let flags = cell_flags(cell);
                        let width = cell_width(cell);
                        h ^= cell_mix(cp, fg, bg, flags, width);
                        h = h.wrapping_mul(16777619);
                        row_cells.push(json!([cp, fg, bg, flags, width]));
                    }
                    // Past the row's allocated length: blank cell, no style —
                    // hashed as a bare codepoint like the JS host.
                    None => {
                        h ^= 32;
                        h = h.wrapping_mul(16777619);
                        row_cells.push(blank_cell());
                    }
                }
            }
            hashes.push(h);
            let changed = match &prev {
                Some(prev) => prev.get(y).map(|p| *p != h).unwrap_or(true),
                None => true,
            };
            if full || changed {
                payload_rows.push((y, row_cells));
            }
        }

        let cursor = self.cursor_msg();
        let changed = payload_rows.len();
        if full || changed > rows * 6 / 10 {
            self.last_hashes = Some(hashes);
            let mut by_row: Vec<Option<Vec<Value>>> = vec![None; rows];
            for (y, cells) in payload_rows {
                by_row[y] = Some(cells);
            }
            let cells: Vec<Vec<Value>> = by_row
                .into_iter()
                .map(|entry| entry.unwrap_or_else(|| vec![blank_cell(); cols]))
                .collect();
            return json!({ "t": "full", "cols": cols, "rows": rows, "cells": cells, "cursor": cursor });
        }
        self.last_hashes = Some(hashes);
        if changed > 0 {
            let rows_map: serde_json::Map<String, Value> = payload_rows
                .into_iter()
                .map(|(y, cells)| (y.to_string(), Value::Array(cells)))
                .collect();
            return json!({ "t": "rows", "rowsMap": rows_map, "cursor": cursor });
        }
        json!({ "t": "cursor", "cursor": cursor })
    }
}

/// 尺寸钳制：下限 2（vt100 解析器不接受更小的网格），上限 `max`。
fn clamp_dim(value: u16, max: u16) -> u16 {
    value.max(2).min(max)
}

/// 网格解析与 diff 帧的单元测试：调色板取值、全量/增量帧、尺寸钳制
/// 与宽字符宽度。
#[cfg(test)]
mod tests {
    use super::*;

    /// 从 `full`/`rows` 帧中取出指定行，把单元格 codepoint 拼成可见
    /// 文本（控制字符替换为空格并去掉尾部空白），便于断言。
    fn row_text(frame: &Value, row: usize) -> String {
        let cells = match frame.get("t").and_then(Value::as_str) {
            Some("rows") => frame["rowsMap"][row.to_string()].as_array(),
            _ => frame["cells"][row].as_array(),
        };
        cells
            .map(|cells| {
                cells
                    .iter()
                    .map(|cell| {
                        let cp = cell[0].as_u64().unwrap_or(32) as u32;
                        if cp >= 32 {
                            char::from_u32(cp).unwrap_or(' ')
                        } else {
                            ' '
                        }
                    })
                    .collect::<String>()
            })
            .unwrap_or_default()
            .trim_end()
            .to_string()
    }

    /// 验证：调色板基色、6×6×6 立方体与灰阶渐变逐点对齐 ghostty-vt。
    #[test]
    fn palette_matches_the_ghostty_aligned_values() {
        // Base: green (idx 2) and bright white (idx 15).
        assert_eq!(palette_color(2), (181 << 16) | (189 << 8) | 104);
        assert_eq!(palette_color(15), 0xffffff);
        // Cube: idx 16 is black, idx 21 is (0,0,255).
        assert_eq!(palette_color(16), 0);
        assert_eq!(palette_color(21), 0x0000ff);
        // Grayscale ramp: idx 232 is 8, idx 255 is 230.
        assert_eq!(palette_color(232), (8 << 16) | (8 << 8) | 8);
        assert_eq!(palette_color(255), 0xeeeeee); // 8 + 23*10 = 238
    }

    /// 验证：首次 drain 无基线，输出整屏 `full` 帧，单元格五元组与
    /// 光标三元组形状正确。
    #[test]
    fn first_drain_emits_a_full_frame_of_the_parsed_screen() {
        let mut grid = GridCore::new(20, 4);
        grid.write(b"\x1b[32mhello\x1b[0m grid");
        let frame = grid.drain();
        assert_eq!(frame["t"], "full");
        assert_eq!(frame["cols"], 20);
        assert_eq!(frame["rows"], 4);
        assert_eq!(row_text(&frame, 0), "hello grid");
        let cell = &frame["cells"][0][0];
        assert_eq!(cell[0], 'h' as u32);
        // Green fg on the first content cell (ghostty-vt aligned palette).
        assert_eq!((cell[1].as_u64().unwrap() >> 16) & 255, 181);
        assert_eq!(cell[4], 1);
        let cursor = frame["cursor"].as_array().unwrap();
        assert_eq!(cursor.len(), 3);
        assert_eq!(cursor[2], true);
    }

    /// 验证：输入未变化时第二次 drain 只发 `cursor` 帧。
    #[test]
    fn second_drain_without_changes_emits_a_cursor_frame() {
        let mut grid = GridCore::new(10, 2);
        grid.write(b"abc");
        assert_eq!(grid.drain()["t"], "full");
        assert_eq!(grid.drain()["t"], "cursor");
    }

    /// 验证：局部改动走 `rows` 增量帧，且只携带真正变化的行。
    #[test]
    fn changed_rows_drain_incrementally() {
        let mut grid = GridCore::new(10, 2);
        grid.write(b"row1");
        assert_eq!(grid.drain()["t"], "full");
        grid.write(b"\r\nrow2");
        let frame = grid.drain();
        assert_eq!(frame["t"], "rows");
        assert!(frame["rowsMap"].get("1").is_some());
        assert_eq!(row_text(&frame, 1), "row2");
    }

    /// 验证：resize 清空哈希基线，下一次 drain 以新尺寸发 `full` 帧。
    #[test]
    fn resize_emits_a_full_frame_at_the_new_dimensions() {
        let mut grid = GridCore::new(20, 4);
        grid.write(b"stale content");
        grid.drain();
        grid.resize(12, 3);
        let frame = grid.drain();
        assert_eq!(frame["t"], "full");
        assert_eq!(frame["cols"], 12);
        assert_eq!(frame["rows"], 3);
    }

    /// 验证：过小/过大的尺寸都被钳到 2..=1000/500，与 JS host 上限一致。
    #[test]
    fn dimensions_clamp_to_the_js_limits() {
        let mut grid = GridCore::new(1, 0);
        assert_eq!((grid.cols(), grid.rows()), (2, 2));
        grid.resize(2000, 600);
        assert_eq!((grid.cols(), grid.rows()), (1000, 500));
        let frame = grid.drain();
        assert_eq!(frame["cols"], 1000);
        assert_eq!(frame["rows"], 500);
    }

    /// 验证：宽字符单元格 width=2，其后继续格 width=0。
    #[test]
    fn wide_characters_report_width_two_and_zero_continuation() {
        let mut grid = GridCore::new(10, 2);
        grid.write("\u{4e2d}\u{6587}".as_bytes());
        let frame = grid.drain();
        let cells = frame["cells"][0].as_array().unwrap();
        assert_eq!(cells[0][0], 0x4e2d);
        assert_eq!(cells[0][4], 2);
        assert_eq!(cells[1][4], 0);
    }

    /// 验证：`full_frame` 重置 diff 基线，无后续输入时下一个 drain
    /// 是 `cursor` 帧。
    #[test]
    fn full_frame_resets_the_diff_baseline() {
        let mut grid = GridCore::new(10, 2);
        grid.write(b"abc");
        grid.drain();
        // fullFrame re-baselines; the next drain with no further input is a
        // cursor frame, exactly like the JS host's snapshot path.
        let full = grid.full_frame();
        assert_eq!(full["t"], "full");
        assert_eq!(grid.drain()["t"], "cursor");
    }

    /// 验证：SGR 粗体/斜体/下划线/反视频属性分别映射到 flags 的
    /// 1/2/4/16 位。
    #[test]
    fn text_attributes_map_to_wire_flags() {
        let mut grid = GridCore::new(10, 1);
        grid.write(b"\x1b[1;3;4;7mX\x1b[0m");
        let frame = grid.drain();
        let cell = &frame["cells"][0][0];
        let flags = cell[3].as_u64().unwrap();
        assert_eq!(flags, 1 | 2 | 4 | 16);
    }
}
