import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import test from 'node:test'

import { validateWorktreeName } from './worktree.mjs'

const run = (args) => spawnSync(process.execPath, ['scripts/worktree.mjs', ...args], { encoding: 'utf8', cwd: new URL('..', import.meta.url).pathname })

test('validateWorktreeName accepts safe names and rejects path escapes', () => {
  assert.equal(validateWorktreeName('fix-foo').ok, true)
  assert.equal(validateWorktreeName('feature.branch_2').ok, true)
  assert.equal(validateWorktreeName('').ok, false)
  assert.equal(validateWorktreeName('../escape').ok, false)
  assert.equal(validateWorktreeName('a/b').ok, false)
  assert.equal(validateWorktreeName('.hidden').ok, false)
  assert.equal(validateWorktreeName('has space').ok, false)
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
