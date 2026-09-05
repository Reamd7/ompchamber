/**
 * @typedef {import('./core.mjs').GridFrame} GridFrame
 * @typedef {import('@lydell/node-pty').IPty} IPty
 */
/**
 * @typedef {object} GridPtySessionOptions
 * @property {number} [cols]
 * @property {number} [rows]
 * @property {string} [shell] Executable path (default: ComSpec or cmd.exe).
 * @property {string} [cwd]
 * @property {Record<string, string | undefined>} [env]
 * @property {string} [name] TERM value reported to the PTY.
 */
export class GridPtySession {
    /** @param {GridPtySessionOptions} [options] */
    constructor({ cols, rows, shell, cwd, env, name }?: GridPtySessionOptions);
    core: GridCore;
    /** @type {IPty} */
    proc: IPty;
    /** Input → PTY. @param {string} data */
    write(data: string): void;
    /** @param {number} cols @param {number} rows */
    resize(cols: number, rows: number): void;
    /** Parsed-grid frames (from the core).
     *  @type {((frame: GridFrame) => void) | null} */
    onFrame: ((frame: GridFrame) => void) | null;
    /** PTY exited. @type {((exitCode: number | undefined) => void) | null} */
    onExit: ((exitCode: number | undefined) => void) | null;
    dispose(): void;
}
export type GridFrame = import("./core.mjs").GridFrame;
export type IPty = import("@lydell/node-pty").IPty;
export type GridPtySessionOptions = {
    cols?: number | undefined;
    rows?: number | undefined;
    /**
     * Executable path (default: ComSpec or cmd.exe).
     */
    shell?: string | undefined;
    cwd?: string | undefined;
    env?: Record<string, string | undefined> | undefined;
    /**
     * TERM value reported to the PTY.
     */
    name?: string | undefined;
};
import { GridCore } from './core.mjs';
