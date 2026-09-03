# The bun-pty Windows stall

This document records a Windows-only defect in `bun-pty` (the PTY backend used
when the OpenChamber web server itself runs under Bun), the measurements that
characterize it, and the decision it forced: the web server now launches under
Node everywhere, and Bun is reserved exclusively for the omp host engine.

It is kept for future reference: anyone reconsidering a Bun runtime for the
server, upgrading bun-pty, or diagnosing a "whole app froze after I killed a
command in the terminal" report should start here.

## Summary

On Windows, if a process that is writing heavily inside a terminal — hundreds
of megabytes of output — is terminated with `TerminateProcess`, bun-pty's
native read loop can pin one CPU core at 100% for a backlog-proportional time
(minutes for very large floods). During the stall:

- the server's JS event loop stays idle (heap does not grow, inspector
  `Runtime.evaluate` sometimes answers in milliseconds),
- HTTP stops answering entirely (`/health` times out),
- `SIGINT` is not delivered (the process cannot be stopped gracefully;
  `--cpu-prof` profiles are never flushed),
- RSS climbs slowly and then drains when the stall ends by itself.

The stall is self-healing and ends immediately if the session's PTY child
process is killed. It does not reproduce under Node with node-pty, on the
identical kill sequence, on the same machine.

## Reproduction

A zero-OpenChamber repro lives at `artifacts/term-bench/pty-variants.mjs`
(run from `packages/web` with `bun`). It spawns `cmd.exe` through bun-pty,
starts a child that floods stdout at ~14 MiB/s, kills the child with
`Stop-Process` (TerminateProcess) mid-write, and samples the host process CPU:

```
[bun-pty] flood started, warming 4s
[bun-pty] bytes: 57.2 MiB — killing child
[bun-pty] t=2s interval-cpu=95% bytes=72.8MiB   <- stall
[bun-pty] t=4s interval-cpu=1%  bytes=72.8MiB   <- drained (small backlog)
[bun-pty] heavy-kill peak=95% verdict: SPIN
```


## Related input-path finding

Independent of the runtime switch: a `0x03` byte written through the terminal
websocket arrives in the PTY as a raw input byte on Windows under both
bun-pty and node-pty — it does not generate the console `CTRL_C_EVENT`
broadcast that a desktop terminal produces. Foreground processes that only
respond to the console event (DOOM-fire, anything not reading stdin) cannot
be interrupted with Ctrl+C through the embedded terminal; interactive
readline-based tools see the raw byte and may treat it as SIGINT themselves.
Closing or restarting the terminal tab remains the reliable way to stop such
processes. This is worth revisiting when ConPTY input handling in the PTY
backend gains an option to translate `0x03` into a console control event.
Through the real server the flood is larger and so is the stall. With a
DOOM-fire instance producing roughly 300 MB before a hard kill, one core stayed
pinned for 45–90+ seconds while `/health` timed out for the whole window.

## Measurements

All on the same machine (Windows 11, Ryzen 7 8745HS), identical kill sequence
(heavy writer inside the PTY, `Stop-Process` mid-write):

| Configuration | PTY backend | Stall after kill |
|---|---|---|
| Server under Bun | bun-pty | 100% core, 45–90+ s (scales with backlog), HTTP dead |
| Server under Node | node-pty | none — 0% within 5 s, `/health` 200 throughout |
| bun-pty standalone repro, ~70 MB backlog | bun-pty | ~2 s |
| node-pty under Bun | — | crashes immediately (`ERR_SOCKET_CLOSED`; its Windows pipe assumes Node internals) |

Other observations from the investigation:

- `ws.bufferedAmount` is `undefined` on Bun's server WebSocket, and a socket
  that never reads accepts 40 MB in 4 s with no error — this is why terminal
  send-side flow control had to be built on application-level acks.
- Writing `\x03` through the terminal websocket does not generate a console
  Ctrl+C event under bun-pty on Windows; foreground processes that ignore
  stdin cannot be interrupted that way. Under node-pty the same write path
  behaves correctly (this was re-verified after the runtime switch).
- An in-process watchdog cannot contain the stall: the JS loop is starved
  unreliably (timers and inspector sometimes answer, sometimes not), so a
  timer-based recycler demonstrably never fires during real stalls. That
  approach was implemented, measured as dead, and removed.

## How users hit it

- Closing or restarting a terminal while a command is producing massive
  output (the server terminates the PTY tree with a hard kill).
- External task managers killing a noisy child process.
- Automated agents force-killing benchmark or build processes inside the
  terminal.

## Decision

The web server launches under Node everywhere (`bin/cli.js` no longer prefers
Bun when installed; the dev stack's `dev:server`/`dev:server:watch` invoke
`node server/index.js`). node-pty is the terminal backend this selects, which
measured clean against the identical kill sequence. Bun remains the runtime
for the omp host engine only — it is required there by the TypeScript SDK and
launches as a separate process.

If bun-pty ever fixes the stall upstream (and Ctrl+C delivery), the runtime
choice can be revisited; the repro script above is the acceptance test.

## Related artifacts

- `artifacts/term-bench/pty-variants.mjs` — minimal repro (Bun required)
- `artifacts/term-bench/fire-run.mjs`, `fire-sniffer.mjs` — end-to-end
  terminal flood drivers used to characterize the stall through the real server
- `packages/web/server/lib/terminal/DOCUMENTATION.md` — terminal subsystem
  design, including the flood backpressure and ack flow control that bound
  what the server itself buffers
