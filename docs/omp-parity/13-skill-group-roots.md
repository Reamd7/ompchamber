# 第 13 章 · 技能分组目录兼容(Skill Group Roots)

状态:**已实施**(`e8e3f102`,2026-08-28)——双标记命中取后者;实现时修正了本设计的 off-by-one(单数标记 7 字符,substring 偏移须按命中标记各自的长度,原 max()-索引方案会多吃一字符),按命中标记长度偏移;四形态测试覆盖。
日期基线:2026-08-28
上游依据:docs/omp-host-field-loss-fix-plan.md P16 遗留项;P1 修复(技能链贯通)后仅剩的分组缺陷

---

## 1. 问题

`parseSkillGroup`(useSkillsStore.ts:102-111)以 `lastIndexOf('/skills/')`(复数)锚定分组路径段。遗留的**单数**根目录(`~/.opencode/skill`、`OPENCODE_CONFIG_DIR/skill`;服务器 skills.js:45-63/624-626 的 managed roots 与扫描均收单复数)下的分组布局技能(`<group>/<name>/SKILL.md`)解析不出 group——设置页技能列表里这些技能失去分组头,平铺在根级。**仅影响展示分组,不影响发现/调用**。

## 2. 方案

`parseSkillGroup` 的锚点从单一定界符改为**取两者中更靠后的命中**:

```ts
const idx = Math.max(
  normalizedPath.lastIndexOf('/skills/'),
  normalizedPath.lastIndexOf('/skill/'),
);
```

理由:路径只可能命中其一(目录名互斥),取 max 等价于"命中存在的那个",无需分支。omp 引擎根(`.omp/skills`、`~/.omp/agent/skills`)是复数,行为不变;单数遗留根获得分组。

**明确不做**:不改服务器侧 `inferSkillScopeAndSourceFromPath` 的单复数识别(它已同时收 `['skill','skills']`,scope 判定无此缺陷);不迁移单数目录到复数(用户文件,不动)。

## 3. 验证

1. 单测:四组路径——复数根分组/复数根平铺/单数根分组/单数根平铺——断言 group 解析结果。
2. 全门禁(vitest)。

## 4. 依赖

无;半小时量级,随任意 UI 批次顺带。
