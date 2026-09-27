/**
 * 配额格式化函数测试套件：覆盖 formatResetTime / calculateResetAfterSeconds
 * 对无效时间戳的容错，以及 toUsageWindow 的派生规则——剩余百分比只从
 * 有限的已用百分比推导、超 100% 钳到 0。
 */
import { describe, expect, it } from 'vitest';

import { calculateResetAfterSeconds, formatResetTime, toUsageWindow } from './formatters.js';

// formatResetTime：无效时间戳必须返回 null（不抛错、不产出 NaN 标签）。
describe('formatResetTime', () => {
  it('returns null for invalid timestamps', () => {
    expect(formatResetTime('not-a-date')).toBeNull();
    expect(formatResetTime(NaN)).toBeNull();
    expect(formatResetTime(Infinity)).toBeNull();
    expect(formatResetTime(-Infinity)).toBeNull();
  });
});

// calculateResetAfterSeconds：epoch 时间戳可用；无效输入返回 null。
describe('calculateResetAfterSeconds', () => {
  it('accepts an epoch reset timestamp', () => {
    expect(calculateResetAfterSeconds(0)).toBe(0);
  });

  it('returns null for invalid timestamps', () => {
    expect(calculateResetAfterSeconds('not-a-date')).toBeNull();
    expect(calculateResetAfterSeconds(NaN)).toBeNull();
    expect(calculateResetAfterSeconds(Infinity)).toBeNull();
    expect(calculateResetAfterSeconds(-Infinity)).toBeNull();
  });
});

// toUsageWindow：重置时间的派生字段与 remainingPercent 的钳制规则。
describe('toUsageWindow', () => {
  it('formats epoch reset timestamps', () => {
    const usageWindow = toUsageWindow({ resetAt: 0 });

    expect(usageWindow.resetAfterSeconds).toBe(0);
    expect(usageWindow.resetAtFormatted).toBe(formatResetTime(0));
    expect(usageWindow.resetAfterFormatted).toBe(formatResetTime(0));
  });

  it('does not derive remaining percent from missing usage', () => {
    expect(toUsageWindow({ usedPercent: undefined }).remainingPercent).toBeNull();
  });

  it('does not derive remaining percent from non-finite usage', () => {
    expect(toUsageWindow({ usedPercent: NaN }).remainingPercent).toBeNull();
    expect(toUsageWindow({ usedPercent: Infinity }).remainingPercent).toBeNull();
    expect(toUsageWindow({ usedPercent: -Infinity }).remainingPercent).toBeNull();
    expect(toUsageWindow({ usedPercent: null }).remainingPercent).toBeNull();
  });

  it('derives remaining percent from a valid usage value', () => {
    expect(toUsageWindow({ usedPercent: 60 }).remainingPercent).toBe(40);
  });

  it('clamps remaining percent to zero when usage exceeds 100', () => {
    expect(toUsageWindow({ usedPercent: 110 }).remainingPercent).toBe(0);
  });
});
