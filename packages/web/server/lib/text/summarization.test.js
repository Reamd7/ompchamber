/**
 * （中文套件说明）文本摘要测试：TTS 清洗的代码剔除顺序、不再调用已下线
 * 的 zen provider（reason 固定为不可用），以及 note/notification 的本地兜底行为。
 */
import { describe, expect, it } from 'vitest';

import { sanitizeForTTS, summarizeText } from './summarization.js';

// 文本摘要兜底：清洗规则与 provider 下线后的固定返回。
describe('text summarization stubs', () => {
  it('removes code from TTS text before stripping markdown punctuation', () => {
    expect(sanitizeForTTS('Read `const value = 1` aloud')).toBe('Read aloud');
    expect(sanitizeForTTS('Before\n```js\nconst value = 1\n```\nAfter')).toBe('Before After');
  });

  it('does not call the retired zen provider', async () => {
    const result = await summarizeText({
      text: 'The implementation now correctly loads notification templates before dispatching the notification. It also fetches the latest assistant message when the event payload does not include message parts. This should make completion notifications match user settings.',
      threshold: 0,
      maxLength: 80,
      zenModel: 'gpt-5-nano',
      mode: 'notification',
    });

    expect(result.summarized).toBe(false);
    expect(result.reason).toBe('Model summarization provider unavailable');
    expect(result.summary).toBe('The implementation now correctly loads notification templates before dispatchin…');
  });

  it('returns local note fallback while provider is unavailable', async () => {
    const result = await summarizeText({
      text: 'First sentence. Second sentence with the useful insight.',
      threshold: 0,
      maxLength: 100,
      mode: 'note',
    });

    expect(result).toMatchObject({
      summary: 'First sentence.',
      summarized: false,
      reason: 'Model summarization provider unavailable',
    });
  });
});
