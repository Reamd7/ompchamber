import fs from 'fs';
import os from 'os';
import path from 'path';

// The walkthrough may run on a different model than the rest of the small-model
// callers. Those callers want cheap and fast; this one needs structured output
// and enough context for a whole diff, and forcing one setting to serve both
// means the user has to degrade one feature to fix the other.

/**
 * walkthrough 专用模型设置模块：从全局 settings.json 读取导览生成用的
 * 模型覆盖项。导览需要 structured output 和装得下整个 diff 的上下文，
 * 与追求便宜快速的其它 small-model 调用方分开设置，避免互相牵制。
 */
/**
 * settings.json 的绝对路径：优先环境变量 OMPCHAMBER_DATA_DIR 指定的数据
 * 目录，否则回落到 ~/.config/ompchamber；在模块加载时求值一次。
 */
const SETTINGS_FILE = path.join(
  process.env.OMPCHAMBER_DATA_DIR
    ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
    : path.join(os.homedir(), '.config', 'ompchamber'),
  'settings.json',
);

/**
 * The explicit walkthrough model, or `null` to fall back to normal small-model
 * resolution.
 *
 * Having chosen a model *is* the opt-out; a separate toggle would let the two
 * disagree, and then clearing the picker would leave a setting that says "do
 * not use the small model" with nothing to use instead.
 *
 * 中文补充：返回值为模型 id 字符串；null 表示回落到常规 small-model 解析。
 * 文件缺失、不可读、非法 JSON，以及空串与纯空白（UI 清空选择器时写入）
 * 一律按无覆盖处理，绝不抛错。
 */
export function readWalkthroughModelOverride() {
  try {
    const settings = JSON.parse(fs.readFileSync(SETTINGS_FILE, 'utf8'));
    if (!settings || typeof settings !== 'object') return null;
    const override = typeof settings.walkthroughModelOverride === 'string'
      ? settings.walkthroughModelOverride.trim()
      : '';
    return override || null;
  } catch {
    // No settings file, unreadable, or malformed all mean the same thing: no
    // override, use the small model.
    return null;
  }
}
