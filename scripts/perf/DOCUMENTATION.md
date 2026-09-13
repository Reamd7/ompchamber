# Performance Measurement Tooling

Owns the unattended performance capture commands and their shared Chrome
DevTools Protocol plumbing. Read this before measuring OpenChamber performance
or extending these scripts. The methodology rules they enforce come from
`.agents/skills/performance-engineering/SKILL.md`.

## Commands

| Command | Answers |
|---|---|
| `bun run profile:idle` | What the app does while nobody interacts with it. |
| `bun run profile:session` | What receiving and rendering a live assistant response costs. |
| `bun run profile:animation` | What a CSS animation costs, isolated from the app. |
| `bun run profile:switch` | How long switching sessions from the sidebar takes, cold and warm. || `bun run profile:browser` | A manually driven capture, for interactions that cannot be scripted. |
| `bun run profile:switch` | What switching between two sessions in the sidebar costs: wall time to a stable message list, long-task distribution, main-thread blockage, and CPU self time. |
| `bun run bench:terminal` | How fast a terminal scrolls (`cat` 500 MiB) and repaints (DOOM-fire fps). Runs inside the terminal under test, not over CDP. |
| `bun scripts/perf/session-open-bench.ts` | What one server-side session load allocates and retains: open, full projection, read-only loader, live materialize/dispose, event-ring burst — each with baseline/steady/quiescence memory intervals. |
| `node scripts/perf/process-tree-sample.mjs` | Whether a pid's child processes return to baseline after spawn/exit or accumulate — counted in the tree, never inferred from RSS. |

The `profile:*` commands measure a real browser over CDP; `bench:terminal`
measures the terminal it runs inside. Pass `--help` to any of them for the
full option list.

## Before Measuring Anything

**Measure a production build.** A development build's render and bundle
behaviour does not represent what users run.

```bash
bun run build:ui && bun run build:web
cd <a project directory> && node <repo>/packages/web/bin/cli.js serve --port 4599 --foreground
```

`profile:idle` and `profile:session` need a running server; `profile:animation`
serves its own fixture and needs nothing.

## profile:idle

Loads the app, lets it settle, then records a window during which no input is
delivered. Everything it reports is therefore work the app performs while the
user is doing nothing — the class of regression users notice as fan noise,
battery drain, and a permanently busy tab.

Reports per second of idle time: main-thread busy time, script, style
recalculation and layout time and counts, DOM node / document / frame /
listener growth, heap trajectory including a least-squares growth rate, a CPU
sampling profile with self time per function, and attribution of timer,
animation-frame and observer work to the call site that scheduled it.

```bash
# Baseline, then compare a change against it and fail on a budget.
bun run profile:idle -- --url http://127.0.0.1:4599 --output artifacts/before
bun run profile:idle -- --url http://127.0.0.1:4599 --baseline artifacts/before --budget-cpu 5
```

Scenario options reach a specific mounted state, because idle cost depends on
what is mounted: `--session`, `--tab`, `--panel <mode>`, `--expand-projects`,
`--expand-sessions`, and `--then-tab` (navigate away after settling, to measure
what a surface keeps doing once the user has left it).

## profile:session

Creates a session, opens it in a browser, dispatches a prompt through the
supported `openchamber session` CLI, and records until the session reports
itself idle. No input is synthesised; the prompt is the only stimulus.

Streaming is judged by responsiveness, not totals, so the report leads with the
long-task distribution, a timeline-trace breakdown naming where time went,
running animations, the application's own stream counters, and output-normalised
metrics.

```bash
bun run profile:session -- --url http://127.0.0.1:4599 --dir <project directory>
# What an idle session costs while a different session is active elsewhere:
bun run profile:session -- --view-session <idle session id> --expand-projects --expand-sessions
```

This command calls a real model. Use a cheap one; `--model` overrides the
configured selection.

## profile:animation

Serves an isolated fixture and measures each animation variant directly, so a
comparison takes seconds instead of an application rebuild plus a streamed
response.

```bash
bun run profile:animation
bun run profile:animation -- --variant border-color --count 8
```

Measured on this repository's fixture, at any element count from 1 to 32:

| Animated property | Style recalculations/sec | Layouts/sec |
|---|---|---|
| none | 0 | 0 |
| `transform` (rotate, translate, scale) | 0 | 0 |
| `opacity`, `filter` | 0 | 0 |
| `rotate` (the individual property) | 60 | 0 |
| `background-position` | 60 | 0 |

| `box-shadow` | 60 | 0 |
| `width` | 60 | 60 |

Animate `transform` and `opacity`. Anything else recalculates style on every
frame for as long as the animation runs, and geometry properties add layout on
top. Note that `rotate: 360deg` is *not* equivalent to
`transform: rotate(360deg)` in cost.

Add a variant to `animation-fixture.html` to measure a property or technique
that is not listed.

## profile:switch

Clicks sidebar session rows with real mouse input and measures, per click, the
two moments a user feels: `ack`, when the clicked row is highlighted as active
(the first visible reaction), and `content`, when the timeline shows messages
that were not on screen before. It also reports the longest main-thread task
inside each switch and every request the switch triggered, so fan-out
regressions show up next to the latency they cause.

Every session in the plan is visited twice. The first visit is usually cold
(a network round trip for messages); the second is warm, served from the
in-memory session store. They have different budgets and are reported
separately.

```bash
bun run profile:switch -- --url http://127.0.0.1:4599 --output artifacts/switch-before
bun run profile:switch -- --url http://127.0.0.1:4599 --baseline artifacts/switch-before --budget-ack 32 --budget-content 100
```

`--sessions a,b,c` picks the rows to click; the default is the first rows in
the sidebar, so pass explicit ids to compare runs across days. The row must be
present in the sidebar; the command fails rather than measuring a click on
nothing.

## terminal-bench

`bun run bench:terminal -- [--label NAME]` (or `node scripts/terminal-bench.mjs`)
scores whichever terminal it runs inside, on the two-test standard: `cat`
throughput and DOOM-fire frame rate. Use it to compare terminals, or terminal
changes, on one machine. It runs on Windows (cmd, PowerShell, Windows
Terminal, Git Bash), macOS and Linux, and must run in an interactive shell
inside the terminal under test.

The cat test streams a cached 500 MiB fixture of deterministic 80-byte lines
and times `cat`. Pty flow control blocks the writer once the terminal falls
behind, so the wall time is the scroll path end to end: parse, grid damage,
scrollback store, paint, plus at most a few hundred KiB of pipe slack. Screen
and scrollback are cleared before each rep; the score is the best of three
reps in MiB/s and Mlines/s.

The fire test builds const-void/DOOM-fire-zig at `-Doptimize=ReleaseFast`;
a Debug build would benchmark the compiler's codegen, not the terminal. When
the local zig does not match the ref's `.zigversion` pin, the pinned zig is
fetched into the cache and used, and the ref re-picked to match, so every
machine builds the same tested combination. The operator maximizes the
window, watches the cumulative `[ N.NN fps ]` counter settle, presses
Ctrl+C, and types the value back. The script grades it (very poor <=5,
poor <=24, ok, great >=100, upstream definitions) and restores the terminal
state DOOM-fire leaves behind on SIGINT (alt screen, hidden cursor, console
modes).

Each run appends one JSON record to `artifacts/term-bench/results.jsonl`
(gitignored): environment fingerprint, per-rep times, best/median throughput,
fire fps with ref, rev and zig version, and both window sizes. Compare only
records with the same machine, OS, window size and font; scrollback settings
are not recorded, so keep those identical by hand.

Validity notes: the cat test refuses non-tty stdout because a pipe measurement
is meaningless (`--force` exists for mechanics checks only), fire requires a
120x22 window, the clock is checked before timing anything, and the fps is
the operator's transcription of the program's own counter, not a scrape.

## session-open-bench

`bun scripts/perf/session-open-bench.ts` measures what one session load
actually allocates and retains, on the server side, against the same session
file repeated (`--repeat`, default 20; the largest `.jsonl` under
`~/.omp/agent/sessions`, or `--file`/`--root`/`--limit` to pick one, or
`--synthetic[=MB]` to generate a fixture through the SDK's own append API).
It is the phase-0 instrument for the omp-host memory plan (docs/plan.md §9.2):
*how much retained and temporary memory do the host's session-load paths
contribute per operation — open, full projection, read-only loader, live
materialize/dispose, event ring — and does an explicitly released
`SessionManager` actually return its heap?*

Scenarios (`--scenario name[,name...]`, default all):

| Scenario | Measures |
|---|---|
| `open` | `SessionManager.open` + `getEntries` + explicit `close()`/`releaseRetainedEntries()` — the parse cost every cold read pays today. |
| `projection` | The engine's cold GET path end to end: open → `buildSessionContext({transcript:true})` → `buildTurnStateStamper` → `projectConversation` → close/release. |
| `cold-reader` | The SDK's public read-only loader `loadSessionMessagesReadOnly` (no writer, no session lock). |
| `ring` | `RingEventBus` burst of `message.part.updated`-sized events under production caps; asserts entry AND byte caps hold, that over-budget events skip the ring, and reports per-emit µs plus `stats()`. |
| `live-materialize` | `createAgentSession` → `beginDispose()` → `dispose()` round-trips. When a headless session cannot boot (no resolvable model), it prints an explicit `skipped: requires …` row — absence is reported, never faked as zero. |

Every scenario records three intervals from `process.memoryUsage()`
(heapUsed/external/rss): **baseline** (after a forced full GC, before work),
**steady** (after the first iteration — rows 2..n), and **quiescence** (after
the final close/release + full GC + one settle). `heapReturnedMB` is
baseline − quiescence heapUsed (negative ⇒ net retained); `rssHighWaterMB` is
the RSS non-return and is allocator high-water — never leak evidence and never
proof of release (plan §2.2). Warmup opens are untimed and explicitly
closed/released; a manager never outlives its scenario. `retainedBytes` from
the ring is a declared serialized-size estimate, not a JS-heap measure.

```bash
bun scripts/perf/session-open-bench.ts --repeat 20                      # largest real session
bun scripts/perf/session-open-bench.ts --synthetic --repeat 3           # no real sessions on this machine
bun scripts/perf/session-open-bench.ts --scenario ring --ring-events 10000
```

Artifacts: `artifacts/session-open-bench/results.jsonl` (one row per
iteration; the ring reports per bucket of emits) and `summary.json` (one
summary block per scenario). The bench proves allocation/retention behavior
of the measured scenarios only — it does not attribute production-host
dominators (plan §9.2); that needs the diagnostics endpoint plus
`process-tree-sample`.

## process-tree-sample

`node scripts/perf/process-tree-sample.mjs` samples the descendant tree of a
pid (`--pid`, default self) every `--interval` ms (default 1000) until
`--duration` ms (default 60000) or SIGINT, printing one JSONL row per sample
(`{t, totalChildren, byName, depth}`) and a final summary
(`{maxChildren, finalChildren, samples, backend}`). It is the phase-0
instrument for the plan's worker question: *do child processes (workers, tool
subprocesses, detached runs) return to baseline after spawn/disconnect/
shutdown, or do they accumulate?* That must be proven by counting the tree —
never inferred from RSS or commit, which the plan (§2) explicitly refuses as
leak evidence.

```bash
node scripts/perf/process-tree-sample.mjs --pid <host pid> --interval 1000 --duration 300000
node scripts/perf/process-tree-sample.mjs --out worker-leak.jsonl          # also persists under artifacts/process-tree/
node scripts/perf/process-tree-sample.mjs --smoke                          # self-test
```

Backends, no new dependencies: Windows uses one PowerShell
`Get-CimInstance Win32_Process` probe per sample (falling back to wmic, then
to a tasklist flat count that is reported as flat — `totalChildren: null` —
never passed off as a tree); POSIX uses `ps -eo pid=,ppid=,comm=,args=`. The
sampler prunes its own probe process from each snapshot so it never counts
itself. A Windows PowerShell probe costs roughly 0.3–1 s per sample, so very
small `--interval` values cannot be honored — samples take as long as the
backend takes. PPID reuse can graft unrelated orphans onto the tree for a
single sample; treat lone spikes without a matching spawn as suspect.
`--smoke` spawns a short-lived child, asserts it appears and that the count
returns to baseline after exit, and exits non-zero on failure.

## Reading The Results

Every run writes a JSON summary next to any raw capture, so results can be
compared later without re-running:

- `profile:idle` → `idle-summary.json`, `cpu-profile.cpuprofile`
- `profile:session` → `session-summary.json`, `cpu-profile.cpuprofile`
- `profile:switch` → `switch-summary.json`, `cpu-profile.cpuprofile`

`--baseline <directory>` prints a per-metric delta table against a previous run
of the same command. `--budget-*` options make the command exit non-zero, so the
same invocation works as an investigation tool and as a regression gate.

Artifacts can reveal project paths and endpoint names. They are gitignored; do
not publish them without review.

## Validity Guarantees

These commands fail loudly rather than reporting a clean result, because each
of these failure modes once produced a confident, wrong "everything is fast":

- **Throttled renderer.** Chrome stops producing frames and throttles timers for
  windows it considers backgrounded or occluded. Launch flags disable that, and
  every run measures frame liveness and warns when the renderer was not
  producing frames.
- **Missing trace data.** `RunTask` is only emitted under the
  disabled-by-default timeline category. A capture without it would report zero
  long tasks; the missing-task case is reported instead.
- **A scenario that never ran.** A session belonging to a directory the browser
  is not viewing renders nothing and produces a perfectly quiet profile.
  `profile:session` verifies both new message elements in the DOM and
  message-list render counters before believing a quiet result.

Preserve this property when extending these scripts. A metric reading zero must
be a measurement, never a disabled instrument.

## Methodology Rules

- **Never report an "after" without a "before" on the identical scenario and
  build.** Rebuild the unchanged version and re-run it, however inconvenient.
  Expect plausible fixes to change nothing.
- **A sampling profiler cannot explain native work.** Self time in `(program)`
  only means the time was not in interpreted JavaScript. Use the trace
  breakdown, which names parsing, style, layout, layerization, paint and raster.
- **Normalise when the workload varies.** Assistant responses differ in length
  between runs, so per-second totals are not comparable; `profile:session`
  reports output-normalised metrics for this reason.
- **Revert what you cannot measure.** A change that does not move its target
  metric is unvalidated complexity, not a small win.
- **Reproduction may need production scale you do not have.** A threshold effect
  is invisible below its threshold. Compare the reporter's scale against yours
  on the dimension the code keys on before concluding a bug is absent.

## Module Layout

| File | Responsibility |
|---|---|
| `cdp.mjs` | Chrome launch, target discovery, minimal CDP client. Owns the anti-throttling launch flags. |
| `metrics.mjs` | Metric derivations shared by the profilers: growth rates, percentiles, long-task and trace-event summaries. |
| `cpu-profile.mjs` | Aggregates `Profiler.stop()` output into self time per function. |
| `idle-probe.mjs` | Page-side instrumentation installed before application code runs; attributes scheduled work to the call site that scheduled it. Must never change observable behaviour. |
| `scenario.mjs` | Shared scenario setup, currently sidebar expansion. Setup always runs before the measured window. |
| `animation-fixture.html` | Isolated animation variants for `profile:animation`. |
| `session-open-bench.ts` | Phase-0 omp-host memory bench: per-scenario session-load allocation/retention with baseline/steady/quiescence intervals (docs/plan.md §9.2). |
| `process-tree-sample.mjs` | Phase-0 child-process tree sampler with platform backends and a `--smoke` self-test. |
