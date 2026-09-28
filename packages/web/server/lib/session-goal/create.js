/**
 * 会话目标的创建流程：把超长 objective 压进上限（先用小模型蒸馏成完成
 * 判据，失败则首尾截断加标记）、优先把 objective 写入按 session id 命名
 * 的文件（失败回退内联 metadata）、生成新 goal id 与初始计数后 PATCH 到
 * 会话 metadata.ompchamber.goal。另提供建目标时注入对话的
 * system-reminder 介绍文案。
 */
import { GOAL_OBJECTIVE_CHAR_LIMIT, writeObjective } from './objectives.js';

/** 蒸馏失败时首尾截断目标文本所插入的说明标记。 */
const TRIM_MARKER = '\n\n[… objective trimmed for the auditor — the full prompt was delivered in the chat message …]\n\n';

/** 构造目标模式开启时注入对话的 <system-reminder> 介绍文案；设置了预算时附上 token 预算说明行。 */
export const buildGoalIntroText = (tokenBudget) => {
  const budgetLine = tokenBudget
    ? ` A token budget of ${tokenBudget} tokens applies to this goal.`
    : '';
  return '<system-reminder>\n'
    + 'Goal mode is active for this session. The user message above defines the goal objective. '
    + 'Work toward it across turns; whenever you stop before the objective is verifiably complete, the system will automatically prompt you to continue. '
    + 'Progress is evaluated independently after each turn, so end every turn with a clear, factual statement of what is done, what was verified, and what remains.'
    + budgetLine
    + '\n</system-reminder>';
};

/**
 * 把超长 objective 压到 GOAL_OBJECTIVE_CHAR_LIMIT 以内：先用小模型把任务
 * 蒸馏成“完成判据”（保留路径 / 命令 / 标识符原文、同语言、控制在 4000
 * 字符内，且限制在与会话相同的 provider 下）；蒸馏失败或产出为空则退回
 * 首尾对半截断并插入 TRIM_MARKER。未超长时原样返回。
 */
const fitObjective = async ({ objective, directory, providerID, modelID, warn }) => {
  if (objective.length <= GOAL_OBJECTIVE_CHAR_LIMIT) return objective;

  let distilled = null;
  try {
    const { generateSmallModelText } = await import('../small-model/index.js');
    const generated = await generateSmallModelText({
      restrictToPreferredProvider: true,
      prompt: objective,
      system: [
        'You distill a large task description into the COMPLETION CRITERIA a progress auditor will judge against.',
        'Return ONLY the criteria text — no preamble, no headers, no markdown fences.',
        'Capture: the end goals, what must exist and work when the task is fully done, and how each major part is verified. Omit implementation steps.',
        'Preserve verbatim any file paths, commands, and identifiers that define the task.',
        'Stay under 4000 characters.',
        'Write in the same language as the task text.',
      ].join('\n'),
      directory,
      preferredProviderID: providerID,
      preferredModelID: modelID,
    });
    distilled = typeof generated?.text === 'string' ? generated.text.trim() : null;
  } catch (error) {
    warn('goal objective distillation failed', error);
  }

  if (distilled) return distilled.slice(0, GOAL_OBJECTIVE_CHAR_LIMIT);
  const half = Math.max(0, Math.floor((GOAL_OBJECTIVE_CHAR_LIMIT - TRIM_MARKER.length) / 2));
  return `${objective.slice(0, half)}${TRIM_MARKER}${objective.slice(-half)}`;
};

/**
 * 创建并激活一个会话目标：objective 经 fitObjective 裁剪后优先写目标
 * 文件（objectiveFile 标记为 true，metadata 只存空 objective）；写文件
 * 失败回退内联截断文本。生成新 goal id 与全部归零的计数，PATCH 会话
 * metadata.ompchamber.goal；PATCH 非 2xx 抛错。onWarning 缺省时警告走
 * console.warn。
 *
 * @returns {Promise<object>} 写入的 goal 对象
 */
export const createSessionGoal = async ({
  baseUrl,
  authHeaders,
  sessionID,
  directory,
  objective,
  tokenBudget = null,
  providerID,
  modelID,
  onWarning,
}) => {
  /** 警告转发：优先 onWarning 回调，否则落到 console.warn。 */
  const warn = (message, error) => {
    if (typeof onWarning === 'function') {
      onWarning(message, error);
      return;
    }
    console.warn(`[session-goal] ${message}:`, error?.message || error);
  };
  const objectiveText = await fitObjective({
    objective: String(objective ?? '').trim(),
    directory,
    providerID,
    modelID,
    warn,
  });
  if (!objectiveText) throw new Error('goal objective is required');

  let objectiveFile = false;
  try {
    await writeObjective(sessionID, objectiveText);
    objectiveFile = true;
  } catch (error) {
    warn('goal objective file write failed, falling back to inline', error);
  }

  const now = Date.now();
  const goal = {
    id: `${now.toString(36)}${Math.random().toString(36).slice(2, 8)}`,
    objective: objectiveFile ? '' : objectiveText.slice(0, GOAL_OBJECTIVE_CHAR_LIMIT),
    objectiveFile,
    status: 'active',
    tokenBudget: tokenBudget || null,
    tokensUsed: 0,
    turnsUsed: 0,
    blockedStreak: 0,
    note: '',
    statusReason: '',
    lastAccountedMessageID: '',
    createdAt: now,
    updatedAt: now,
  };
  const url = new URL(`${baseUrl}/session/${encodeURIComponent(sessionID)}`);
  url.searchParams.set('directory', directory);
  const response = await fetch(url.toString(), {
    method: 'PATCH',
    headers: {
      ...authHeaders,
      'content-type': 'application/json',
      accept: 'application/json',
    },
    body: JSON.stringify({ metadata: { ompchamber: { goal } } }),
  });
  if (!response.ok) throw new Error(`goal metadata patch failed (${response.status})`);
  return goal;
};
