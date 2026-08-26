/**
 * One-time migration of the legacy OpenChamber data directory
 * (~/.config/openchamber) to the ompchamber layout (~/.config/ompchamber).
 *
 * The whole tree moves in a single rename so every consumer (settings, auth,
 * pairing state, projects, chats, quota credentials, ...) switches over
 * atomically. A failed rename leaves the legacy directory untouched so the
 * next boot retries. When both directories exist the current one wins and the
 * legacy directory is left in place for manual reconciliation.
 */
import fs from 'fs';
import os from 'os';
import path from 'path';

const legacyDataDir = () => path.join(os.homedir(), '.config', 'openchamber');
const currentDataDir = () => path.join(os.homedir(), '.config', 'ompchamber');

let migrationAttempted = false;

export function migrateLegacyAppDataDir({ logger = console } = {}) {
  if (migrationAttempted) return;
  migrationAttempted = true;

  // OPENCHAMBER_DATA_DIR selects an isolated data directory (tests, dev); the
  // user's home layout must not be touched in that mode.
  if (typeof process.env.OPENCHAMBER_DATA_DIR === 'string' && process.env.OPENCHAMBER_DATA_DIR.trim()) {
    return;
  }

  const legacyDir = legacyDataDir();
  const currentDir = currentDataDir();
  try {
    if (!fs.existsSync(legacyDir)) return;
    if (fs.existsSync(currentDir)) {
      logger.warn(`[ompchamber] keeping existing data directory ${currentDir}; legacy ${legacyDir} left in place`);
      return;
    }
    fs.renameSync(legacyDir, currentDir);
    logger.log(`[ompchamber] migrated data directory ${legacyDir} -> ${currentDir}`);
  } catch (error) {
    logger.warn(`[ompchamber] could not migrate data directory ${legacyDir} -> ${currentDir}: ${(error && error.message) || error}`);
  }
}
