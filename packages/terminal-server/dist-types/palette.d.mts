/**
 * Resolve an xterm palette index to packed 24-bit RGB.
 * @param {number} idx Palette index 0-255.
 * @returns {number} 0xRRGGBB.
 */
export function paletteColor(idx: number): number;
/** Default foreground as packed 24-bit RGB (0xRRGGBB). @type {number} */
export const DEFAULT_FG: number;
/** Default background as packed 24-bit RGB. @type {number} */
export const DEFAULT_BG: number;
/**
 * RGB triple, 0-255 per channel.
 */
export type Rgb = readonly [number, number, number];
