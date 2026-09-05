export class GridCore {
    /** @param {GridCoreOptions} [options] */
    constructor({ cols, rows, maxCols, maxRows }?: GridCoreOptions);
    maxCols: number;
    maxRows: number;
    cols: number;
    rows: number;
    term: headless.Terminal;
    /** @type {number[] | null} */
    lastHashes: number[] | null;
    /** Called with a frame after each parsed batch.
     *  @type {((frame: GridFrame) => void) | null} */
    onFrame: ((frame: GridFrame) => void) | null;
    _drainQueued: boolean;
    /** Feed VT bytes. Frames arrive via onFrame once parsing completes.
     *  @param {string | Uint8Array} data */
    write(data: string | Uint8Array): void;
    /** @param {number} cols @param {number} rows */
    resize(cols: number, rows: number): void;
    /** @returns {CursorMsg} */
    cursorMsg(): CursorMsg;
    _queueDrain(): void;
    /**
     * Force a complete frame (resets the diff baseline). Hosts use this to
     * materialize snapshots for newly attaching clients; the next drain()
     * diffs against this point.
     * @returns {FullFrame}
     */
    fullFrame(): FullFrame;
    /** Compute the next diff frame. Also the sync API for hosts that prefer pull.
     *  @returns {GridFrame} */
    drain(): GridFrame;
    dispose(): void;
}
/**
 * One parsed cell, fixed shape:
 * [codepoint, fg(24bit), bg(24bit), flags, width]
 * flags bits match ghostty-web CellFlags: 1 bold, 2 italic, 4 underline,
 * 8 strike, 16 inverse, 32 invisible, 128 faint.
 */
export type CellData = [number, number, number, number, number];
/**
 * Cursor as [x, y, visible].
 */
export type CursorMsg = [number, number, boolean];
/**
 * Complete screen state; also what snapshots reconcile from.
 */
export type FullFrame = {
    t: "full";
    cols: number;
    rows: number;
    cells: CellData[][];
    cursor: CursorMsg;
};
/**
 * Incremental row diff; keys are row indices as decimal strings (JSON
 * object keys).
 */
export type RowsFrame = {
    t: "rows";
    rowsMap: Record<string, CellData[]>;
    cursor: CursorMsg;
};
export type CursorFrame = {
    t: "cursor";
    cursor: CursorMsg;
};
/**
 * Any grid frame variant.
 */
export type GridFrame = FullFrame | RowsFrame | CursorFrame;
export type GridCoreOptions = {
    /**
     * Initial columns (default 80).
     */
    cols?: number | undefined;
    /**
     * Initial rows (default 24).
     */
    rows?: number | undefined;
    /**
     * Column clamp (default 1000 — matches the
     * OpenChamber runtime's terminal dimension validation).
     */
    maxCols?: number | undefined;
    /**
     * Row clamp (default 500).
     */
    maxRows?: number | undefined;
};
import headless from '@xterm/headless';
