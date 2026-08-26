import assert from 'node:assert/strict'
import test from 'node:test'
import { parseNetstatListeningPids, renderReport } from './stop-dev.mjs'

const NETSTAT_FIXTURE = [
  'Active Connections',
  '',
  '  Proto  Local Address          Foreign Address        State           PID',
  '  TCP    0.0.0.0:5180           0.0.0.0:0              LISTENING       111844',
  '  TCP    0.0.0.0:5180           192.168.1.4:51022      ESTABLISHED     111844',
  '  TCP    [::]:5180              [::]:0                 LISTENING       111845',
  '  TCP    127.0.0.1:3902         0.0.0.0:0              LISTENING       83988',
  '  TCP    127.0.0.1:9050         127.0.0.1:9050         CLOSE_WAIT      4242',
  '  TCP    0.0.0.0:445            0.0.0.0:0              LISTENING       4',
  '  UDP    127.0.0.1:5180         *:*                                    1234',
  '',
].join('\r\n')

test('parseNetstatListeningPids maps each wanted port to its listening pids', () => {
  const listeners = parseNetstatListeningPids(NETSTAT_FIXTURE, [5180, 3902])
  assert.deepEqual(
    {
      pids5180: [...listeners.get(5180)].sort(),
      pids3902: [...listeners.get(3902)],
    },
    { pids5180: [111844, 111845], pids3902: [83988] },
  )
})

test('parseNetstatListeningPids ignores non-LISTENING rows but keeps system pids for callers to filter', () => {
  const listeners = parseNetstatListeningPids(NETSTAT_FIXTURE, [9050, 445])
  assert.equal(listeners.has(9050), false)
  assert.deepEqual([...listeners.get(445)], [4])
})

const OUTCOME = {
  ok: false,
  results: [
    { port: 5180, origin: 'default', pids: [111844], outcome: 'stopped' },
    { port: 3902, origin: 'default', pids: [], outcome: 'free' },
    { port: 5183, origin: '.worktrees/x/.dev-ports.json', pids: [222], outcome: 'busy' },
  ],
}

test('renderReport --json emits a single JSON payload with per-port outcomes', () => {
  const report = renderReport(OUTCOME, { jsonMode: true })
  const payload = JSON.parse(report.out)
  assert.deepEqual(payload, {
    status: 'error',
    stoppedCount: 1,
    busyCount: 1,
    results: OUTCOME.results,
  })
  assert.equal(report.err, '')
  assert.equal(report.exitCode, 1)
})

test('renderReport --quiet puts free/stopped on stdout and busy on stderr', () => {
  const report = renderReport(OUTCOME, { quietMode: true })
  assert.equal(report.out, 'port 5180 stopped pid=111844\nport 3902 free\n')
  assert.equal(report.err, 'port 5183 busy pid=222\n')
  assert.equal(report.exitCode, 1)
})

test('renderReport --quiet carries tool errors without stdout noise', () => {
  const report = renderReport({ ok: false, error: 'lsof is unavailable', results: [] }, { quietMode: true })
  assert.equal(report.out, '')
  assert.equal(report.err, 'lsof is unavailable\n')
  assert.equal(report.exitCode, 1)
})

test('renderReport --json ok outcome exits zero', () => {
  const report = renderReport(
    { ok: true, results: [{ port: 5180, origin: 'default', pids: [], outcome: 'free' }] },
    { jsonMode: true },
  )
  assert.equal(JSON.parse(report.out).status, 'ok')
  assert.equal(report.exitCode, 0)
})
