import { describe, expect, test } from 'bun:test';
import { resolveProjectForSessionDirectory } from './projectResolution';

const projects = [
  { id: 'ompchamber', path: '/workspace/ompchamber', label: 'OMPChamber' },
];

describe('resolveProjectForSessionDirectory', () => {
  test('resolves a sibling worktree to its registered project', () => {
    const worktrees = new Map([
      ['/workspace/ompchamber', [{
        path: '/workspace/ompchamber-feature',
        projectDirectory: '/workspace/ompchamber',
        branch: 'feature',
        label: 'feature',
      }]],
    ]);

    expect(resolveProjectForSessionDirectory(projects, worktrees, '/workspace/ompchamber-feature')).toEqual(projects[0]);
  });
});
