import assert from 'node:assert/strict'
import fs from 'node:fs'
import { fileURLToPath } from 'node:url'
import { spawnSync } from 'node:child_process'
import test from 'node:test'

import { validateBranchName, validateWorktreeName, worktreeDirName } from './worktree.mjs'

const repoRoot = fileURLToPath(new URL('..', import.meta.url))
const run = (args) => spawnSync(process.execPath, ['scripts/worktree.mjs', ...args], { encoding: 'utf8', cwd: repoRoot })

test('validateWorktreeName accepts safe names and rejects path escapes', () => {
  assert.equal(validateWorktreeName('feature/foo').ok, true)
  assert.equal(validateWorktreeName('fix-foo').ok, true)
  assert.equal(validateWorktreeName('feature.branch_2').ok, true)
  assert.equal(validateWorktreeName('').ok, false)
  assert.equal(validateWorktreeName('../escape').ok, false)
  assert.equal(validateWorktreeName('.hidden').ok, false)
  assert.equal(validateWorktreeName('has space').ok, false)
  assert.equal(validateWorktreeName('back\\slash').ok, false)
})

test('worktreeDirName maps every "/" to "_" for the .worktrees/ directory', () => {
  assert.equal(worktreeDirName('feature/foo'), 'feature_foo')
  assert.equal(worktreeDirName('hotfix/a/b'), 'hotfix_a_b')
  assert.equal(worktreeDirName('plain'), 'plain')
})

test('validateBranchName accepts GitHub-legal worktree branches', () => {
  for (const ok of [
    'feature/fix-foo', 'hotfix/foo-bar_v1.2', 'feature/a', 'hotfix/v1.2.3', 'feature/a/b/c',
    'feature/foo@', 'feature/foo./bar', 'feature/issue-12_noble-raccoon', 'feature/ünïcode',
    'hotfix/2026-08',
  ]) {
    assert.equal(validateBranchName(ok).ok, true, ok)
  }
})

test('validateBranchName rejects each git check-ref-format rule with a reason', () => {
  const cases = [
    ['', /branch name is required/],
    ['-foo', /must not start with "-"/],
    ['@', /must not be just "@"/],
    ['HEAD', /"HEAD" is reserved/],
    ['foo bar', /must not contain spaces/],
    ['foo:bar', /must not contain spaces/],
    ['foo~bar', /must not contain spaces/],
    ['foo^bar', /must not contain spaces/],
    ['foo?bar', /must not contain spaces/],
    ['foo*bar', /must not contain spaces/],
    ['foo[bar', /must not contain spaces/],
    ['foo\\bar', /must not contain spaces/],
    ['foo\x01bar', /must not contain spaces/],
    ['foo\x7fbar', /must not contain spaces/],
    ['foo..bar', /must not contain "\.\."/],
    ['foo@{bar', /must not contain "@\{"/],
    ['foo.', /must not end with "\."/],
    ['foo.lock', /must not end with "\.lock"/],
    ['foo/bar.lock', /must not end with "\.lock"/],
    ['/foo', /must not start or end with "\/"/],
    ['foo/', /must not start or end with "\/"/],
    ['foo//bar', /or contain "\/\/"/],
    ['.foo', /path segments must not start with "\."/],
    ['foo/.bar', /path segments must not start with "\."/],
    ['fix-foo', /must start with "feature\/" or "hotfix\/"/],
    ['release/2026-08', /must start with "feature\/" or "hotfix\/"/],
    ['Feature/foo', /must start with "feature\/" or "hotfix\/"/],
  ]
  for (const [bad, why] of cases) {
    const check = validateBranchName(bad)
    assert.equal(check.ok, false, JSON.stringify(bad))
    assert.match(check.reason, why, JSON.stringify(bad))
  }
})

test('missing subcommand exits 2 with usage and never prompts (non-TTY)', () => {
  const result = run([])
  assert.equal(result.status, 2)
  assert.match(result.stderr, /usage/)
})

test('missing name exits 2 in non-TTY instead of hanging on a prompt', () => {
  const result = run(['init'])
  assert.equal(result.status, 2)
  assert.match(result.stderr, /name is required/)
})

test('invalid name exits 2 with the reason', () => {
  const result = run(['init', 'has space'])
  assert.equal(result.status, 2)
  assert.match(result.stderr, /invalid name/)
})

test('name without feature/ or hotfix/ prefix exits 2 and creates nothing', () => {
  const result = run(['init', 'fix-foo'])
  assert.equal(result.status, 2)
  assert.match(result.stderr, /invalid branch "fix-foo" \(must start with "feature\/" or "hotfix\/"\)/)
  assert.equal(
    fs.existsSync(fileURLToPath(new URL('../.worktrees/fix-foo', import.meta.url))),
    false,
  )
})

test('feature/ name passes name and branch validation (stopped at bad base)', () => {
  const result = run(['init', 'feature/wt-guard-probe-9f3', '--base', 'wt-no-such-ref-9f3'])
  assert.equal(result.status, 2)
  assert.match(result.stderr, /base ref "wt-no-such-ref-9f3" not found/)
})

test('json mode reports usage errors as a JSON payload', () => {
  const result = run(['frobnicate', '--json'])
  assert.equal(result.status, 2)
  const payload = JSON.parse(result.stdout)
  assert.match(payload.error, /usage/)
})

test('invalid --branch exits 2 with the reason and creates nothing', () => {
  const result = run(['init', 'branch-guard-probe', '--branch', 'bad branch'])
  assert.equal(result.status, 2)
  assert.match(result.stderr, /invalid branch "bad branch"/)
  assert.equal(
    fs.existsSync(fileURLToPath(new URL('../.worktrees/branch-guard-probe', import.meta.url))),
    false,
  )
})

test('name that is an illegal derived branch is rejected before side effects', () => {
  const result = run(['init', 'feature/bad..name'])
  assert.equal(result.status, 2)
  assert.match(result.stderr, /invalid branch "feature\/bad\.\.name"/)
  assert.equal(
    fs.existsSync(fileURLToPath(new URL('../.worktrees/feature_bad..name', import.meta.url))),
    false,
  )
})

test('json mode reports invalid branch as a JSON payload', () => {
  const result = run(['init', 'branch-guard-probe', '--branch', 'x@{y', '--json'])
  assert.equal(result.status, 2)
  const payload = JSON.parse(result.stdout)
  assert.match(payload.error, /invalid branch/)
})
