/**
 * agent 页面截图的落盘与命名（openchamber-control/screenshots）。
 *
 * 图片写在服务端、紧挨它所佐证的代码，因为持有仓库的是这台机器——
 * 按下快门的客户端可能远在别处。写进项目的文件也是唯一能活过这次
 * 对话的形式：可被引用、提交或附到 review 上。无人能定位的截图不算
 * 证据，所以文件名带上 agent 选择的标签与拍照时刻，并把页面与布局
 * 信息一并交回调用方。
 */
/**
 * Where an agent's page screenshots land.
 *
 * The image is written on the server, next to the code it is evidence for,
 * because that is the machine holding the repository — the client that took the
 * picture may be somewhere else entirely. A file in the project is also the
 * only form of this that survives past the chat: it can be referenced from an
 * answer, committed, or attached to a review.
 *
 * A screenshot nobody can place is not evidence, so the name carries the label
 * the agent chose and the moment it was taken, and the caller is handed back
 * the page and layout it shows.
 */
import path from 'node:path';
import fsPromises from 'node:fs/promises';

/** Project-relative home for agent screenshots. */
/** 中文补充：截图在项目内的相对根目录（.ompchamber/screenshots）。 */
export const SCREENSHOT_DIRECTORY = path.join('.ompchamber', 'screenshots');

/** 标签 slug 的最大长度（字符数），超出截断。 */
const MAX_LABEL_LENGTH = 48;

/**
 * Turns a label into a filename fragment.
 *
 * Everything outside a small safe set is dropped rather than escaped: this
 * value reaches the filesystem, and a label is a name, never a path. `..`, a
 * separator, or a leading dot cannot survive this.
 */
/**
 * 中文补充：把标签转成文件名片段。安全集合之外的字符一律丢弃而非
 * 转义——该值会到达文件系统，而标签是名字、不是路径：`..`、路径分
 * 隔符与前导点都无法在转换后幸存。
 */
export const screenshotSlug = (label) => {
  const slug = String(label ?? '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, MAX_LABEL_LENGTH)
    .replace(/-+$/g, '');
  return slug || 'page';
};

/** File-safe timestamp: sorts chronologically and reads as a date. */
/** 中文补充：文件名安全的时间戳：按时间排序且可读作日期。 */
const screenshotStamp = (date) => date.toISOString().replace(/[:.]/g, '-').replace('Z', '');

/** MIME 类型 → 文件扩展名映射；未知类型回退 .jpg。 */
const EXTENSIONS = new Map([
  ['image/jpeg', '.jpg'],
  ['image/png', '.png'],
  ['image/webp', '.webp'],
]);

/**
 * Writes one capture into the project and reports where it went.
 *
 * Returns both the project-relative path — what belongs in an answer or a
 * commit — and the absolute one, so a caller that needs the file itself does
 * not have to rebuild it.
 */
/**
 * 中文补充：把一次截图写入项目并报告落点。同时返回项目相对路径
 * （属于回答或提交的那份）与绝对路径（需要文件本身的调用方不必再拼）。
 * directory 为空或 base64 为空时直接抛错，不写零字节文件。
 */
export const writeScreenshot = async ({
  directory,
  base64,
  mime = 'image/jpeg',
  label,
  now = new Date(),
  fs = fsPromises,
}) => {
  if (typeof directory !== 'string' || directory.trim().length === 0) {
    throw new Error('A project directory is required to save a screenshot');
  }
  if (typeof base64 !== 'string' || base64.length === 0) {
    throw new Error('The browser returned no image');
  }

  const extension = EXTENSIONS.get(mime) || '.jpg';
  const relativePath = path.join(
    SCREENSHOT_DIRECTORY,
    `${screenshotSlug(label)}-${screenshotStamp(now)}${extension}`,
  );
  const absolutePath = path.join(directory, relativePath);

  await fs.mkdir(path.dirname(absolutePath), { recursive: true });
  await fs.writeFile(absolutePath, Buffer.from(base64, 'base64'));

  // Posix separators in the reported path: it is written into Markdown and
  // commit messages, where a Windows separator is an escape character.
  return { path: relativePath.split(path.sep).join('/'), absolutePath };
};
