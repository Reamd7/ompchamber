/**
 * 磁盘缓存读写工具：把 JSON 对象持久化到 OMPChamber 数据目录
 * （环境变量 OMPCHAMBER_DATA_DIR，缺省 ~/.config/ompchamber）。写入走
 * 临时文件 + 原子 rename；读取与写入失败均静默降级（内存缓存仍是权威）。
 */
import fs from 'fs';
import os from 'os';
import path from 'path';

/** 解析数据目录：优先 OMPCHAMBER_DATA_DIR 环境变量，否则用 ~/.config/ompchamber。 */
const resolveDataDir = () => (process.env.OMPCHAMBER_DATA_DIR
  ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
  : path.join(os.homedir(), '.config', 'ompchamber'));

/** 同步读取并解析 JSON 文件；缺失、不可读、解析失败或非普通对象时返回 null。 */
const readJsonFile = (filePath) => {
  try {
    const raw = fs.readFileSync(filePath, 'utf8');
    const parsed = JSON.parse(raw);
    return parsed && typeof parsed === 'object' && !Array.isArray(parsed) ? parsed : null;
  } catch {
    return null;
  }
};

/**
 * Read a persisted cache object from the OMPChamber data directory.
 * Returns null when the file is missing, unreadable, or malformed.
 */
/** 读取数据目录下的缓存对象；文件缺失、不可读或格式非法一律返回 null。 */
export const readDiskCache = (fileName) => {
  try {
    return readJsonFile(path.join(resolveDataDir(), fileName));
  } catch {
    return null;
  }
};

/**
 * Persist a cache object to the OMPChamber data directory with an atomic
 * temp-file rename. Failures are ignored: the in-memory cache stays
 * authoritative and the next successful write retries persistence.
 */
/** 原子写入缓存（临时文件权限 0600 + rename）；失败清理临时文件并返回 false。 */
export const writeDiskCache = (fileName, data) => {
  const filePath = path.join(resolveDataDir(), fileName);
  const tempPath = `${filePath}.${process.pid}.${Date.now()}.tmp`;
  try {
    fs.mkdirSync(path.dirname(filePath), { recursive: true });
    fs.writeFileSync(tempPath, JSON.stringify(data), { encoding: 'utf8', mode: 0o600 });
    fs.renameSync(tempPath, filePath);
    return true;
  } catch {
    try {
      fs.unlinkSync(tempPath);
    } catch {
      // ignore
    }
    return false;
  }
};
