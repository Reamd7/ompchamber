/** 精选技能源清单（curated-sources.js）测试：确认 Anthropic 官方源在列。 */
import { describe, expect, it } from 'vitest';
import { getCuratedSkillsSources } from './curated-sources.js';

/** getCuratedSkillsSources 返回清单的内容检查。 */
describe('getCuratedSkillsSources', () => {
  it('includes the Anthropic curated source', () => {
    const anthropic = getCuratedSkillsSources().find((source) => source.id === 'anthropic');
    expect(anthropic).toBeDefined();
    expect(anthropic.label).toBe('Anthropic');
  });
});
