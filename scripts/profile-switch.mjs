#!/usr/bin/env node
/**
 * Automated chat-switch capture for OMPChamber.
 *
 * Answers: what does switching to a session cost on the main thread, measured
 * on a production build without DevTools attached? The user-visible scenario
 * is a sidebar click, so that is the only stimulus — no synthetic rendering,
 * no direct store calls.
 *
 * Reports, per switch and aggregated: wall time from click to a stable message
 * list, long-task distribution inside that window, main-thread blockage, and a
 * CPU sampling profile with self time per function. Warns loudly when the
 * renderer was throttled or the scenario never opened a session, instead of
 * reporting a clean zero.
 *
 * Usage:
 *   bun run build:ui && bun run build:web
 *   cd <project dir> && node <repo>/packages/web/bin/cli.js serve --port 4599 --foreground
 *   bun run profile:switch -- --url http://127.0.0.1:4599 --to <session id or title substring>
 *
 * Without --from the first other visible session row is used as the starting
 * point. --repeat runs ping-pong switches and reports median/p95.
 */

import { mkdir, writeFile } from "node:fs/promises"
import { join, resolve } from "node:path"
import process from "node:process"

import { CdpClient, createPageTarget, evaluateValue, launchChrome, reservePort, resolveChrome, wait } from "./perf/cdp.mjs"
import { summarizeCpuProfile } from "./perf/cpu-profile.mjs"

const HELP = `Usage: bun run profile:switch -- [options]

Options:
  --url <url>            App URL served from a production build (required)
  --from <id|title>      Session to switch away from (default: first visible row)
  --to <id|title>        Session to switch into (required)
  --settle <seconds>     Settle time after load before the first switch (default 12)
  --stable <seconds>     Message list must be unchanged for this long (default 1.5)
  --timeout <seconds>    Per-switch stabilization timeout (default 90)
  --repeat <count>       Ping-pong switches to record (default 3)
  --output <dir>         Artifact directory (default artifacts/switch-profile-<ts>)
  --sampling-interval <us> CPU sampling interval (default 1000)
  --headless <bool>      Headless Chrome (default true)
  --chrome <path>        Chrome executable
  --profile-dir <path>   Chrome user-data dir
  --json                 Print the summary as JSON

Exit code is non-zero when no session switch could be recorded.`

const parseArgs = (argv) => {
  const options = {
    settle: 12,
    stable: 1.5,
    timeout: 90,
    repeat: 3,
    samplingInterval: 1000,
    headless: true,
    profileDir: join(process.env.TEMP ?? process.cwd(), "oc-perf-chrome-profile"),
    output: null,
    url: null,
    from: null,
    to: null,
    json: false,
    help: false,
  }
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index]
    const value = argv[index + 1]
    const num = (fallback) => {
      const parsed = Number(value)
      index += 1
      return Number.isFinite(parsed) ? parsed : fallback
    }
    if (flag === "--help" || flag === "-h") options.help = true
    else if (flag === "--url") { options.url = value; index += 1 }
    else if (flag === "--from") { options.from = value; index += 1 }
    else if (flag === "--to") { options.to = value; index += 1 }
    else if (flag === "--settle") options.settle = num(options.settle)
    else if (flag === "--stable") options.stable = num(options.stable)
    else if (flag === "--timeout") options.timeout = num(options.timeout)
    else if (flag === "--repeat") options.repeat = Math.max(1, num(options.repeat))
    else if (flag === "--sampling-interval") options.samplingInterval = num(options.samplingInterval)
    else if (flag === "--output") { options.output = value; index += 1 }
    else if (flag === "--profile-dir") { options.profileDir = value; index += 1 }
    else if (flag === "--headless") { options.headless = value !== "false"; index += 1 }
    else if (flag === "--chrome") { options.chrome = value; index += 1 }
    else if (flag === "--json") options.json = true
    else throw new Error(`Unknown option: ${flag}`)
  }
  if (!options.help) {
    if (!options.url) throw new Error("--url is required")
    if (!options.to) throw new Error("--to is required")
  }
  return options
}

// Page-side helpers, injected as one expression each. The observer must be
// installed before the click so long tasks are never missed.
const INSTALL_OBSERVER = `(() => {
  if (globalThis.__switchObserver) return 'installed'
  const entries = []
  const observer = new PerformanceObserver((list) => {
    for (const entry of list.getEntries()) {
      entries.push({ type: entry.entryType, start: entry.startTime, duration: entry.duration })
    }
  })
  observer.observe({ entryTypes: ['longtask', 'resource'] })
  globalThis.__switchObserver = { entries }
  return 'installed'
})()`


const FIND_ROW = (key) => `(() => {
  const wanted = ${JSON.stringify(key)}
  const rows = Array.from(document.querySelectorAll('[data-session-row]'))
  if (rows.length === 0) return { error: 'no session rows in sidebar' }
  for (const row of rows) {
    const id = row.getAttribute('data-session-row') ?? ''
    if (id && (id === wanted || id.startsWith(wanted))) {
      return { id, title: (row.textContent ?? '').trim().slice(0, 80) }
    }
  }
  const byTitle = rows.find((row) => ((row.textContent ?? '').includes(wanted)))
  if (byTitle) {
    return { id: byTitle.getAttribute('data-session-row'), title: (byTitle.textContent ?? '').trim().slice(0, 80) }
  }
  return { error: 'no matching row', rowCount: rows.length }
})()`

const CLICK_ROW = (id) => `(() => {
  const row = document.querySelector('[data-session-row="' + ${JSON.stringify(id)} + '"]')
  if (!row) return false
  row.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
  return true
})()`

const STATE = (targetId) => `(() => {
  const messages = document.querySelectorAll('[data-message-id]').length
  return {
    url: location.href,
    onTarget: location.href.includes(${JSON.stringify(targetId)}),
    messages,
    now: performance.now(),
    observerCount: (globalThis.__switchObserver?.entries ?? []).length,
  }
})()`

const COLLECT_LONG_TASKS = (fromMs, toMs) => `(() => {
  const entries = (globalThis.__switchObserver?.entries ?? [])
    .filter((entry) => entry.type === 'longtask' && entry.start >= ${fromMs} - 5 && entry.start <= ${toMs})
    .map((entry) => ({ start: entry.start, duration: Math.round(entry.duration) }))
    .sort((a, b) => b.duration - a.duration)
  globalThis.__switchObserver.entries.length = 0
  return entries
})()`

const percentiles = (values, fraction) => {
  if (values.length === 0) return 0
  const sorted = [...values].sort((left, right) => left - right)
  const index = Math.min(sorted.length - 1, Math.ceil(fraction * sorted.length) - 1)
  return sorted[Math.max(0, index)]
}

const round = (value, digits = 1) => Number(Number(value).toFixed(digits))

const main = async () => {
  const options = parseArgs(process.argv.slice(2))
  if (options.help) {
    console.log(HELP)
    return
  }

  const chrome = resolveChrome(options.chrome)
  const timestamp = new Date().toISOString().replaceAll(":", "-").replaceAll(".", "-")
  const output = resolve(options.output ?? join("artifacts", `switch-profile-${timestamp}`))
  await mkdir(output, { recursive: true })
  await mkdir(resolve(options.profileDir), { recursive: true })

  const port = await reservePort()
  const chromeProcess = launchChrome({ chrome, profileDir: options.profileDir, port, headless: options.headless })
  let client
  try {
    const target = await createPageTarget(port)
    client = new CdpClient(target.webSocketDebuggerUrl)
    await client.connect()
    await Promise.all([
      client.send("Page.enable"),
      client.send("Runtime.enable"),
      client.send("Performance.enable"),
      client.send("Profiler.enable"),
      client.send("Network.enable", { maxTotalBufferSize: 0, maxResourceBufferSize: 0 }),
    ])
    await client.send("Network.setBypassServiceWorker", { bypass: true })
    await client.send("Emulation.setDeviceMetricsOverride", {
      width: 1600,
      height: 1000,
      deviceScaleFactor: 1,
      mobile: false,
    })

    const loaded = client.once("Page.loadEventFired", 60_000)
    await client.send("Page.navigate", { url: options.url })
    await loaded
    console.log(`Loaded ${options.url}; settling ${options.settle}s for session bootstrap.`)
    await wait(options.settle * 1000)

    await evaluateValue(client, INSTALL_OBSERVER)

    const fromRow = await evaluateValue(client, FIND_ROW(options.from ?? ""))
    if (fromRow.error && options.from) throw new Error(`--from session not found: ${JSON.stringify(fromRow)}`)
    const toRow = await evaluateValue(client, FIND_ROW(options.to))
    if (toRow.error) throw new Error(`--to session not found: ${JSON.stringify(toRow)}`)
    if (fromRow.id === toRow.id) throw new Error("--from and --to resolved to the same session")

    // Open the starting session and let it stabilize before recording.
    if (fromRow.id) {
      await evaluateValue(client, CLICK_ROW(fromRow.id))
      await wait(3000)
    }

    console.log(`Switching: ${JSON.stringify(fromRow.title ?? "(none)")} -> ${JSON.stringify(toRow.title)}`)

    await client.send("Profiler.setSamplingInterval", { interval: options.samplingInterval })
    await client.send("Profiler.start")

    const switches = []
    let current = { id: toRow.id, key: options.to }
    let previous = { id: fromRow.id, key: options.from ?? "" }
    for (let step = 0; step < options.repeat; step += 1) {
      // Always measure switching INTO the --to session when odd, back into the
      // from session when even, so ping-pong rounds stay comparable.
      const target = step % 2 === 0 ? toRow : fromRow
      if (!target.id) break
      await wait(1500)
      const clickAt = await evaluateValue(client, `performance.now()`)
      const clicked = await evaluateValue(client, CLICK_ROW(target.id))
      if (!clicked) throw new Error(`session row vanished: ${target.id}`)

      const deadline = Date.now() + options.timeout * 1000
      let stableSince = null
      let lastCount = -1
      let finalState = null
      while (Date.now() < deadline) {
        await wait(250)
        const state = await evaluateValue(client, STATE(target.id))
        finalState = state
        if (state.onTarget && state.messages > 0 && state.messages === lastCount) {
          if (stableSince === null) stableSince = Date.now()
          if (Date.now() - stableSince >= options.stable * 1000) break
        } else {
          stableSince = null
          lastCount = state.messages
        }
      }
      const doneAt = await evaluateValue(client, `performance.now()`)
      const opened = Boolean(finalState?.onTarget && (finalState?.messages ?? 0) > 0)
      const longTasks = opened
        ? await evaluateValue(client, COLLECT_LONG_TASKS(clickAt, doneAt))
        : []
      const blockMs = longTasks.reduce((total, task) => total + task.duration, 0)
      switches.push({
        into: target.id,
        title: target.title,
        opened,
        wallMs: round(doneAt - clickAt),
        messages: finalState?.messages ?? 0,
        longTasks,
        blockMs: round(blockMs),
        worstTaskMs: longTasks.length > 0 ? longTasks[0].duration : 0,
      })
      const entry = switches[switches.length - 1]
      console.log(
        `  switch ${step + 1}/${options.repeat} -> ${(target.title ?? target.id).slice(0, 40).padStart(40)}`
          + ` wall ${String(entry.wallMs).padStart(7)}ms`
          + ` block ${String(entry.blockMs).padStart(7)}ms`
          + ` worst ${String(entry.worstTaskMs).padStart(7)}ms`
          + ` msgs ${entry.messages}`,
      )
      previous = current
      current = { id: target.id }
    }

    const { profile } = await client.send("Profiler.stop")

    // Renderer-throttling guard, mirroring profile-idle.
    const frameLiveness = await evaluateValue(client, `new Promise((resolve) => {
      let frames = 0
      const startedAt = performance.now()
      const tick = () => {
        frames += 1
        if (performance.now() - startedAt < 1000) requestAnimationFrame(tick)
        else resolve({ framesPerSecond: frames, visibilityState: document.visibilityState })
      }
      requestAnimationFrame(tick)
      setTimeout(() => resolve({ framesPerSecond: frames, visibilityState: document.visibilityState }), 2000)
    })`)

    const recorded = switches.filter((entry) => entry.opened)
    if (recorded.length === 0) {
      throw new Error("no switch stabilized inside the timeout; refusing to report a clean result")
    }
    const summary = {
      url: options.url,
      to: { id: toRow.id, title: toRow.title },
      from: { id: fromRow.id, title: fromRow.title },
      switches,
      wallMs: {
        median: round(percentiles(recorded.map((entry) => entry.wallMs), 0.5)),
        p95: round(percentiles(recorded.map((entry) => entry.wallMs), 0.95)),
        max: round(Math.max(...recorded.map((entry) => entry.wallMs))),
      },
      blockMs: {
        median: round(percentiles(recorded.map((entry) => entry.blockMs), 0.5)),
        p95: round(percentiles(recorded.map((entry) => entry.blockMs), 0.95)),
        max: round(Math.max(...recorded.map((entry) => entry.blockMs))),
      },
      worstTaskMs: {
        median: round(percentiles(recorded.map((entry) => entry.worstTaskMs), 0.5)),
        max: round(Math.max(...recorded.map((entry) => entry.worstTaskMs))),
      },
      longTasksOver100Ms: recorded.reduce((total, entry) => total + entry.longTasks.filter((task) => task.duration >= 100).length, 0),
      frameLiveness,
      cpu: summarizeCpuProfile(profile),
    }

    await writeFile(join(output, "switch-summary.json"), JSON.stringify(summary, null, 2))
    await writeFile(join(output, "cpu-profile.cpuprofile"), JSON.stringify(profile))

    if (Number(frameLiveness?.framesPerSecond ?? 0) < 10) {
      console.warn(`WARNING: renderer produced ${frameLiveness?.framesPerSecond ?? 0} fps; this run understates real work.`)
    }

    if (options.json) console.log(JSON.stringify(summary, null, 2))
    else {
      console.log(`\nSwitch wall ms : median ${summary.wallMs.median} p95 ${summary.wallMs.p95} max ${summary.wallMs.max}`)
      console.log(`Main-thread blocked ms : median ${summary.blockMs.median} p95 ${summary.blockMs.p95} max ${summary.blockMs.max}`)
      console.log(`Worst long task ms : median ${summary.worstTaskMs.median} max ${summary.worstTaskMs.max}`)
      console.log(`Long tasks >=100ms across switches: ${summary.longTasksOver100Ms}`)
      console.log(`\nTop CPU self time during switches:`)
      for (const row of summary.cpu.topSelfTime.slice(0, 15)) {
        console.log(`  ${String(row.selfMs).padStart(9)} ms  ${String(row.percentOfBusy).padStart(6)}%  ${row.function}`)
      }
    }
    console.log(`\nSaved to ${output}`)
  } finally {
    client?.close()
    if (!chromeProcess.killed) chromeProcess.kill("SIGTERM")
  }
}

main().catch((error) => {
  console.error(`Switch profiling failed: ${error.message}`)
  process.exit(1)
})
