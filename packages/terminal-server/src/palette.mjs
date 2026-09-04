/**
 * Palette + color resolution matching ghostty-vt's built-in theme
 * (Tomorrow-style; values cross-checked by the xterm.js differential
 * harness in references/ghostty-web). Used to resolve xterm palette
 * indices to the RGB ghostty cells carry.
 */

const BASE = [
  [204, 204, 204], [204, 102, 102], [181, 189, 104], [222, 147, 95],
  [129, 162, 190], [178, 148, 187], [138, 190, 183], [204, 204, 204],
  [117, 117, 117], [241, 141, 133], [219, 200, 106], [233, 190, 126],
  [138, 178, 235], [213, 161, 216], [148, 216, 209], [255, 255, 255],
];

const PALETTE = BASE.slice();
for (let i = 16; i < 232; i++) {
  const c = i - 16;
  const steps = [0, 95, 135, 175, 215, 255];
  PALETTE.push([
    steps[Math.floor(c / 36)],
    steps[Math.floor((c % 36) / 6)],
    steps[c % 6],
  ]);
}
for (let i = 232; i < 256; i++) {
  const g = 8 + (i - 232) * 10;
  PALETTE.push([g, g, g]);
}

export const DEFAULT_FG = (204 << 16) | (204 << 8) | 204;
export const DEFAULT_BG = 0;

export function paletteColor(idx) {
  const p = PALETTE[idx >>> 0] ?? PALETTE[0];
  return (p[0] << 16) | (p[1] << 8) | p[2];
}
