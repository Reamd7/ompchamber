/**
 * notification message helpers 测试套件。
 *
 * 覆盖三条路径：truncateNotificationText 的超长截断、prepareNotificationLastMessage
 * 对已废弃摘要 settings 的忽略（必须基于原始消息文本截断），以及 Markdown 到
 * 纯文本的归一化（去标记、保留代码内容、折叠空白）。
 */
import { describe, expect, it } from 'vitest';

import { prepareNotificationLastMessage, truncateNotificationText } from './message.js';

/** 验证通知文本的截断与 Markdown 归一化行为。 */
describe('notification message helpers', () => {
  it('truncates oversized notification text', () => {
    expect(truncateNotificationText('abcdef', 3)).toBe('abc...');
  });

  it('ignores retired summarization settings and truncates original message', async () => {
    const result = await prepareNotificationLastMessage({
      message: '0123456789',
      settings: {
        summarizeLastMessage: true,
        summaryThreshold: 5,
        summaryLength: 3,
        maxLastMessageLength: 4,
      },
    });

    expect(result).toBe('0123...');
  });

  it('normalizes markdown message to plain text', async () => {
    const result = await prepareNotificationLastMessage({
      message: "**Committed.**\n\n- Commit: `85924b9d`\n- Message: `fix desktop notifications`",
      settings: {
        maxLastMessageLength: 200,
      },
    });

    expect(result).toBe('Committed. Commit: 85924b9d Message: fix desktop notifications');
  });
});
