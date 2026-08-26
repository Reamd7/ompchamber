import assert from 'node:assert/strict'
import fs from 'node:fs'
import { fileURLToPath } from 'node:url'
import { spawnSync } from 'node:child_process'
import test from 'node:test'

import { validateBranchName, validateWorktreeName } from './worktree.mjs'

const repoRoot = fileURLToPath(new URL('..', import.meta.url))
const run = (args) => spawnSync(process.execPath, ['scripts/worktree.mjs', ...args], { encoding: 'utf8', cwd: repoRoot })

test('validateWorktreeName accepts safe names and rejects path escapes', () => {
  assert.equal(validateWorktreeName('fix-foo').ok, true)
  assert.equal(validateWorktreeName('feature.branch_2').ok, true)
  assert.equal(validateWorktreeName('').ok, false)
  assert.equal(validateWorktreeName('../escape').ok, false)
  assert.equal(validateWorktreeName('a/b').ok, false)
  assert.equal(validateWorktreeName('.hidden').ok, false)
  assert.equal(validateWorktreeName('has space').ok, false)
})

test('validateBranchName accepts GitHub-legal branch names', () => {
  for (const ok of [
    'fix-foo', 'feature/foo-bar_v1.2', 'a', 'v1.2.3', 'a/b/c', 'foo@', 'foo./bar',
    'issue-12_noble-raccoon', 'ünïcode', 'release/2026-08',
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
  const result = run(['init', 'bad/name'])
  assert.equal(result.status, 2)
  assert.match(result.stderr, /invalid name/)
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
  const result = run(['init', 'bad..name'])
  assert.equal(result.status, 2)
  assert.match(result.stderr, /invalid branch "bad\.\.name"/)
  assert.equal(
    fs.existsSync(fileURLToPath(new URL('../.worktrees/bad..name', import.meta.url))),
    false,
  )
})

test('json mode reports invalid branch as a JSON payload', () => {
  const result = run(['init', 'branch-guard-probe', '--branch', 'x@{y', '--json'])
  assert.equal(result.status, 2)
  const payload = JSON.parse(result.stdout)
  assert.match(payload.error, /invalid branch/)
})
