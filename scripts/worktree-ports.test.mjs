import assert from 'node:assert/strict'
import fs from 'node:fs'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import test from 'node:test'

import {
  allocateDevPorts,
  DEFAULT_API_PORT,
  DEFAULT_UI_PORT,
  DEV_PORTS_FILENAME,
  isFreePort,
  readDevPorts,
  writeDevPorts,
} from './worktree-ports.mjs'

const tmpRepo = () => fs.mkdtempSync(path.join(os.tmpdir(), 'wt-ports-'))

test('writeDevPorts + readDevPorts round-trip', () => {
  const dir = tmpRepo()
  writeDevPorts(dir, { uiPort: 5191, apiPort: 3911 })
  assert.deepEqual(readDevPorts(dir), { uiPort: 5191, apiPort: 3911 })
})

test('readDevPorts returns null for missing, malformed, and out-of-range files', () => {
  const dir = tmpRepo()
  assert.equal(readDevPorts(dir), null)

  fs.writeFileSync(path.join(dir, DEV_PORTS_FILENAME), '{not json')
  assert.equal(readDevPorts(dir), null)

  fs.writeFileSync(path.join(dir, DEV_PORTS_FILENAME), JSON.stringify({ uiPort: 5191 }))
  assert.equal(readDevPorts(dir), null)

  fs.writeFileSync(path.join(dir, DEV_PORTS_FILENAME), JSON.stringify({ uiPort: 0, apiPort: 99999 }))
  assert.equal(readDevPorts(dir), null)
})

test('isFreePort is false while the port is bound and true after release', async () => {
  const server = net.createServer()
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
  const { port } = server.address()
  assert.equal(await isFreePort(port), false)
  await new Promise((resolve) => server.close(resolve))
  assert.equal(await isFreePort(port), true)
})

test('allocateDevPorts skips the shared defaults and recorded worktree pairs', async () => {
  const repoRoot = tmpRepo()
  const first = await allocateDevPorts({ repoRoot })
  // Fresh repo: first pair sits directly above the shared defaults.
  assert.equal(first.uiPort, DEFAULT_UI_PORT + 1)
  assert.equal(first.apiPort, DEFAULT_API_PORT + 1)

  const existing = path.join(repoRoot, '.worktrees', 'existing')
  fs.mkdirSync(existing, { recursive: true })
  writeDevPorts(existing, first)
  const second = await allocateDevPorts({ repoRoot })
  assert.equal(second.uiPort, first.uiPort + 1)
  assert.equal(second.apiPort, first.apiPort + 1)
  assert.notEqual(second.uiPort, DEFAULT_UI_PORT)
  assert.notEqual(second.apiPort, DEFAULT_API_PORT)
})

test('allocateDevPorts skips a live listener even when unrecorded', async () => {
  const repoRoot = tmpRepo()
  const server = net.createServer()
  await new Promise((resolve) => server.listen(DEFAULT_UI_PORT + 1, '127.0.0.1', resolve))
  try {
    const ports = await allocateDevPorts({ repoRoot })
    assert.equal(ports.uiPort, DEFAULT_UI_PORT + 2)
  } finally {
    await new Promise((resolve) => server.close(resolve))
  }
})
