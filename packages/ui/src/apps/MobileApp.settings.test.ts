import { describe, expect, test } from 'bun:test';
import fs from 'node:fs';

const source = fs.readFileSync(new URL('./MobileApp.tsx', import.meta.url), 'utf8');

describe('mobile settings availability', () => {
  test('includes the OMP Plugins page in the mobile settings whitelist', () => {
    const whitelist = source.match(/const MOBILE_SETTINGS_PAGES = \[([\s\S]*?)\] as const;/)?.[1] ?? '';
    expect(whitelist).toContain("'plugins'");
  });
});
