/**
 * 技能目录的精选源清单：目录页默认展示的一组可信 GitHub 技能仓库，
 * 含展示名、来源仓库与默认子路径（defaultSubpath，仓库内技能所在的目录）。
 */

/** 精选源常量数组；导出接口返回其浅拷贝以防外部篡改。 */
const CURATED_SKILLS_SOURCES = [
  {
    id: 'anthropic',
    label: 'Anthropic',
    description: "Anthropic's public skills repository",
    source: 'anthropics/skills',
    defaultSubpath: 'skills',
    sourceType: 'github',
  },
  {
    id: 'openai',
    label: 'OpenAI',
    description: "OpenAI's curated skills",
    source: 'openai/skills',
    defaultSubpath: 'skills/.curated',
    sourceType: 'github',
  },
  {
    id: 'cursor',
    label: 'Cursor',
    description: "Cursor's plugin skills",
    source: 'cursor/plugins',
    defaultSubpath: 'pstack/skills',
    sourceType: 'github',
  },
  {
    id: 'mattpocock',
    label: 'Matt Pocock',
    description: 'Matt Pocock skills collection',
    source: 'mattpocock/skills',
    sourceType: 'github',
  },
];

/** 返回精选源列表的浅拷贝，调用方修改不会影响模块内的常量。 */
export function getCuratedSkillsSources() {
  return CURATED_SKILLS_SOURCES.slice();
}
