// Tests for the omp slash-command discovery domain (spec 08 §5.4, master R2).
//
// SDK ground truth asserted here was verified against the installed source:
// - BUILTIN_SLASH_COMMANDS_INTERNAL (slash-commands/builtin-registry.ts:34-40)
//   is the full reserved-name registry — including TUI-only handlers like
//   `debug` (builtin-lifecycle.ts:282-287) which available-commands.ts:44
//   filters out for ACP dispatch but which must stay in the discovery list
//   for collision resolution.
// - buildAvailableSlashCommands(session) (available-commands.ts:31-97)
//   aggregates skills (gated by skillsSettings.enableSkillCommands), extension
//   and custom commands, and file commands from sessionManager.getCwd().

import { describe, test, expect } from 'bun:test';
import {
  OMP_COMMAND_TIERS,
  builtinOmpCommands,
  listOmpCommands,
  projectOmpCommand,
  registerCommandsDomainRoutes,
} from './domain-commands.ts';

describe('projectOmpCommand', () => {
  test('maps source→tier and extracts the argument template', () => {
    expect(projectOmpCommand({
      name: 'review',
      description: 'Run a review',
      input: { hint: '<focus>' },
      source: 'file',
    })).toEqual({
      name: 'review',
      description: 'Run a review',
      tier: 'engine',
      source: 'file',
      argumentHint: '<focus>',
    });
    expect(projectOmpCommand({ name: 'debug', description: 'Open debug tools selector', source: 'builtin' }))
      .toEqual({ name: 'debug', description: 'Open debug tools selector', tier: 'client-builtin', source: 'builtin' });
  });

  test('rejects nameless rows and drops empty optional fields', () => {
    expect(projectOmpCommand({ description: 'x', source: 'file' })).toBeNull();
    expect(projectOmpCommand(null)).toBeNull();
    const projected = projectOmpCommand({ name: 'a', aliases: ['', 'b'], source: 'skill' });
    expect(projected?.aliases).toEqual(['b']);
    expect(projected?.description).toBeUndefined();
  });
});

describe('builtinOmpCommands (Tier A reservation)', () => {
  test('covers the full registry including TUI-only names, all client-builtin', () => {
    const rows = builtinOmpCommands();
    expect(rows.length).toBeGreaterThanOrEqual(60);
    const byName = new Map(rows.map((r) => [r.name, r]));
    expect(byName.get('debug')?.tier).toBe('client-builtin');
    expect(byName.get('compact')?.tier).toBe('client-builtin');
    // Composer-local OC commands must NOT collide: the doc-verified gap
    // (08 §5.4) that keeps init/undo/redo/timeline as Tier C.
    for (const local of ['init', 'undo', 'redo', 'timeline', 'troubleshoot']) {
      expect(byName.has(local)).toBe(false);
    }
    expect(rows.every((r) => OMP_COMMAND_TIERS.includes(r.tier) && typeof r.description === 'string')).toBe(true);
  });
});

describe('listOmpCommands aggregation', () => {
  test('appends engine rows after builtins, name-deduped, directory-scoped', async () => {
    const seenCwds: string[] = [];
    const rows = await listOmpCommands({
      directory: '/repo',
      // filePath/baseDir/source satisfy Skill's required fields; they are
      // never read — the mocked loadAvailable ignores session.skills.
      loadSkills: async () => [{ name: 'web', description: 'web skill', filePath: '/skills/web/SKILL.md', baseDir: '/skills/web', source: 'user' }],
      loadAvailable: async (session) => {
        seenCwds.push(session.sessionManager.getCwd());
        expect(session.skillsSettings?.enableSkillCommands).toBe(true);
        return [
          { name: 'review', description: 'project review command', source: 'file' },
          { name: 'skill:web', description: 'Run web skill', source: 'skill' },
          { name: 'compact', description: 'shadow attempt', source: 'file' },
          // builtin rows from the aggregated loader are skipped: the full
          // registry was already appended above.
          { name: 'settings', description: 'dupe', source: 'builtin' },
        ];
      },
    });
    const byName = new Map(rows.map((r) => [r.name, r]));
    expect(seenCwds).toEqual(['/repo']);
    expect(byName.get('review')).toMatchObject({ tier: 'engine', source: 'file' });
    expect(byName.get('skill:web')).toMatchObject({ tier: 'engine', source: 'skill' });
    // First-seen-wins: the reserved builtin keeps its name.
    expect(byName.get('compact')?.tier).toBe('client-builtin');
    expect(byName.get('settings')?.description).not.toBe('dupe');
    // Builtins lead the list.
    expect(rows[0].tier).toBe('client-builtin');
  });

  test('discovery failure degrades to the builtin list (never empty)', async () => {
    const rows = await listOmpCommands({
      directory: '/repo',
      loadSkills: async () => {
        throw new Error('skills unreadable');
      },
      loadAvailable: async () => {
        throw new Error('boom');
      },
    });
    expect(rows.length).toBe(builtinOmpCommands().length);
    expect(rows.some((r) => r.name === 'debug')).toBe(true);
  });

  test('real SDK aggregation returns unique names with valid tiers', async () => {
    const rows = await listOmpCommands({ directory: process.cwd() });
    const names = new Set(rows.map((r) => r.name));
    expect(names.size).toBe(rows.length);
    expect(rows.every((r) => OMP_COMMAND_TIERS.includes(r.tier))).toBe(true);
    expect(rows.every((r) => typeof r.name === 'string' && r.name.length > 0)).toBe(true);
  });
});

describe('registerCommandsDomainRoutes (commands.v1 gate)', () => {
  const makeRoute = () => {
    const handlers = new Map();
    const route = (method: string, pattern: string, handler: (request: Request, ctx?: { params?: Record<string, string> }) => Promise<Response>) => handlers.set(`${method} ${pattern}`, handler);
    return { handlers, route };
  };
  const request = (url: string) => new Request(url);

  test('capability off answers an explicit 501 without calling the loader', async () => {
    const { handlers, route } = makeRoute();
    let called = 0;
    registerCommandsDomainRoutes(route, {
      features: { 'commands.v1': false },
      list: async () => {
        called += 1;
        return [];
      },
    });
    const response = await handlers.get('GET /omp/commands')(request('http://host/omp/commands'));
    expect(response.status).toBe(501);
    expect(await response.json()).toEqual({ error: 'commands.v1-unavailable' });
    expect(called).toBe(0);
  });

  test('capability on returns the list and threads ?directory=', async () => {
    const { handlers, route } = makeRoute();
    const directories: string[] = [];
    registerCommandsDomainRoutes(route, {
      features: { 'commands.v1': true },
      list: async ({ directory }) => {
        directories.push(directory);
        return [{ name: 'debug', tier: 'client-builtin', source: 'builtin' }];
      },
    });
    const response = await handlers.get('GET /omp/commands')(
      request('http://host/omp/commands?directory=/proj'),
    );
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual([{ name: 'debug', tier: 'client-builtin', source: 'builtin' }]);
    expect(directories).toEqual(['/proj']);
  });
});

// ---------------------------------------------------------------------------
// Live-session extension commands merge (09 §5.4 discovery gap)
// ---------------------------------------------------------------------------

describe('listOmpCommands — loadLiveCommands', () => {
  test('live extension commands append after builtins, deduped by name', async () => {
    const commands = await listOmpCommands({
      directory: '/repo',
      loadAvailable: async () => [],
      loadSkills: async () => [],
      loadLiveCommands: async () => [
        { name: 'zhipu-usage', description: '查询智谱 GLM Coding Plan 用量' },
        { name: 'tps-monitor', description: 'tps stats' },
        { name: 'debug' }, // collides with a builtin — builtin must win
      ],
    });
    const byName = new Map(commands.map((c) => [c.name, c]));
    expect(byName.get('zhipu-usage')).toMatchObject({ tier: 'engine', source: 'extension' });
    expect(byName.get('tps-monitor')).toMatchObject({ tier: 'engine' });
    expect(byName.get('debug')?.tier).toBe('client-builtin');
  });

  test('a throwing live source degrades to the static halves', async () => {
    const commands = await listOmpCommands({
      directory: '/repo',
      loadAvailable: async () => [],
      loadSkills: async () => [],
      loadLiveCommands: async () => {
        throw new Error('no live session');
      },
    });
    expect(commands.length).toBeGreaterThan(0);
    expect(commands.some((c) => c.name === 'zhipu-usage')).toBe(false);
  });

  test('absent loader changes nothing (legacy call shape)', async () => {
    const commands = await listOmpCommands({
      directory: '/repo',
      loadAvailable: async () => [],
      loadSkills: async () => [],
    });
    expect(commands.every((c) => c.name !== 'zhipu-usage')).toBe(true);
  });
});
