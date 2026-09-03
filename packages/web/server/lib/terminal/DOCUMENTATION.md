# Terminal Subsystem

## Ownership

`runtime.js` owns terminal identity, PTY processes, status, ordered output, bounded scrollback, WebSocket attachments, and lifecycle routes. `shells.js` discovers executable shell families and resolves the persisted shell ID without accepting command strings or arguments. Clients own tab arrangement and choose stable terminal IDs. Electron uses this same runtime in-process; VS Code returns an explicit unsupported error.

## Protocol

`/api/terminal/ws` is the only terminal data transport. It uses v3 binary JSON control frames and is opened through `openRuntimeWebSocket`, preserving direct, Electron proxy, URL-token authentication, and private-relay routing.

- `attach` registers a connection for one terminal. One socket may attach to many terminals.
- Every attach and reconnect begins with an authoritative `snapshot` containing bounded history and the current sequence.
- A current socket that closes or errors before its initial `open` invalidates its URL-scoped auth token before retrying, so retries mint a fresh token instead of backing off against a rejected upgrade. Hidden or offline clients wait 60 seconds and wake promptly on visibility/online recovery.
- `output`, `exit`, and `restarted` carry monotonically increasing per-terminal sequences. Output carries raw live bytes plus replay-safe bytes with terminal query exchanges removed.
- Attach registers before capturing the snapshot, buffers concurrent events, drops events represented by the snapshot sequence, then enters live delivery.
- `write` always includes the terminal ID; sockets never have mutable single-terminal binding state.
- `ack` carries one terminal's last applied sequence per frame (`{t:'ack', s, q}`). Clients acknowledge applied sequences at least every 200ms while output streams and immediately after applying a snapshot; the server uses them solely for send-side flow control (below), never for ordering. Attach/snapshot exchanges carry no ordering contract beyond `q`.
- `detach` removes only that attachment.
- Creation carries the active UI appearance. The PTY sets `COLORFGBG` and answers OSC 10, OSC 11, Mode 2031, and primary-device-attribute queries immediately, including queries emitted before a WebSocket attachment exists. The DA1 fallback prevents Fish from waiting ten seconds for a renderer that cannot observe or answer its startup query. Subscribed TUIs receive a Mode 2031 notification when the appearance changes.

HTTP remains the authenticated command plane for create, resize, appearance updates, restart, close, and force-kill. There is no SSE output or HTTP input compatibility path.

`GET /api/terminal/sessions` enumerates live sessions (optionally filtered by resolved `cwd`) so clients can adopt terminals their local tab projection does not know about — another device, a new browser tab, or cleared storage. `POST /api/terminal/touch` refreshes `lastActivity` for the listed session ids; open clients call it periodically so background tabs, which hold no WebSocket attachment, are not idle-reaped while a client still shows them.

## PTY Lifecycle

- IDs are client-provided or generated with `randomUUID()`.
- Concurrent creates for one ID are single-flight only when working directory and shell preference match. Existing IDs cannot be reused for another working directory.
- Dimensions are bounded to 1-1000 columns and 1-500 rows; input is capped at 64 KiB.
- A client may create before its renderer has mounted. It derives an initial size from the container and font metrics (falling back to 80x24 when unavailable), then sends a resize once Ghostty reports its final dimensions. This allows shell startup and renderer initialization to overlap.
- PTY children explicitly clear `NODE_CHANNEL_FD`; daemon IPC descriptors are host-private and invalid after PTY descriptor cleanup.
- PTY children also strip AppImage `ARGV0` (and other host-private shell vars such as `ELECTRON_RUN_AS_NODE`, `BASH_ENV`, `ENV`, `BASH_XTRACEFD`). An exported `ARGV0` makes zsh rewrite argv[0] for every external command, which breaks Python venv detection and other argv[0]/$0 consumers while leaving `/proc/self/exe` correct. On Linux, PTY spawn is wrapped with `env -u ARGV0` because `bun-pty` merges the native OS environ and would otherwise reintroduce `ARGV0` after a JS-only delete.
- `GET /api/terminal/shells` reports shell IDs available on the active server using the same augmented PATH provided to spawned PTYs, plus whether each executable has a supported login-mode argument. `auto` preserves environment/platform fallback order; an explicit unavailable shell fails creation instead of silently running a different shell. Login mode is opt-in and uses only built-in arguments for known shells. Preference changes affect new sessions and explicit restarts, not running PTYs.
- PTY data and exit callbacks enter one FIFO queue. Stale callbacks from replaced processes are ignored. The drain is sliced by bytes and wall time (256 KiB / 4 ms per slice) and yields between slices via `setImmediate`, so a flood cannot monopolize the event loop; restart bumps a drain token that invalidates in-flight slices.
- Scrollback is retained on the server and capped at 512 KiB with UTF-8-safe trimming. Device-status, device-attribute, cursor-position reply, and color-query exchanges are removed from replay history with incomplete control sequences carried across PTY chunks; live output remains byte-for-byte unchanged. History is stored as a chunk deque (append is O(chunk); the byte-exact 512 KiB tail is applied when a snapshot materializes the text), never as one accumulating string — string concat per PTY chunk copies ~512 KiB per ~4 KiB chunk and was the dominant CPU cost under floods.
- Send-side flow control bounds server memory per attachment: Bun's server WebSocket exposes no send-buffer signal and buffers unbounded data per socket, so each attachment tracks sent-but-unacknowledged bytes (output frames and snapshots). Past 4 MiB the attachment is suppressed — output frames are skipped for it while sequence numbers keep advancing, so its next live frame gap-triggers the client's existing resync — and once its lag drains below 1 MiB by acknowledgment it receives a snapshot and live output resumes. Connections that never acknowledge are suppressed after 8 MiB as a fail-safe. Non-output events (exit, resized, restarted, command-finished) are never suppressed. A flood therefore completes at PTY speed, other attachments and HTTP keep their service, and server memory stays bounded regardless of output volume.
- Restarts are serialized per terminal. Each restart spawns and wires the replacement before terminating the old process, retaining the terminal ID.
- Close uses SIGTERM with bounded SIGKILL escalation. Force-kill, idle cleanup, and runtime shutdown terminate process groups immediately where supported. Removal explicitly sends a fatal scoped closure and evicts client projections even when a PTY backend fails to emit `onExit`; attached terminals are not considered idle.
- Session lifetime is claim-scoped for multi-device sharing. `POST
  /api/terminal/touch` with a `claimant` (a per-window client instance id)
  records a claim on each named session; `DELETE
  /api/terminal/:sessionId?claimant=…` is a tab close: it releases exactly
  that claimant's stake and terminates the PTY only when no other live claim
  remains (another window/device still shows the session; claims older than
  the idle window are expired first so crashed clients cannot keep sessions
  undead). A DELETE without `claimant` — the explicit kill button and
  force-kill — terminates unconditionally. Output does not refresh session
  lifetime: an unattached chatty orphan (its client crashed without closing)
  still becomes idle-reapable, while claims/touches/input do refresh it.
- Windows PTY spawn prefers the `conpty.dll` bundled with node-pty's
  prebuilds (a current Terminal-era build) over the OS-built-in
  pseudoconsole: measured 3x read throughput on output floods (42 vs 14
  MiB/s standalone; 43.6 vs 7.4 MiB/s end-to-end). The DLL's presence is
  resolved once at runtime start; a packaging gap silently falls back to
  the built-in path. The bundled DLL probes primary device attributes
  (`ESC [ c`) during its own startup handshake, so the theme-responder only
  answers DA queries on the classic backend (`session.respondPrimaryDA`) —
  answering the DLL's probe wrote the response into the shell's input line.

## Security And Relay

The WebSocket path must remain in both `isUrlAuthWebSocketPath` and relay `ALLOWED_WS_PATHS`. The client must use `getRuntimeUrlResolver().websocket()` and `openRuntimeWebSocket`; direct local URLs or raw browser WebSockets break relay and URL-token authentication.

## Verification

Run:

```sh
bun test packages/web/server/lib/terminal/runtime.test.js packages/web/server/lib/terminal/terminal-ws-protocol.test.js
bun test packages/web/server/lib/ui-auth/ui-auth.test.js packages/web/server/lib/relay/cross-compat.test.js
```

## Known upstream issue: bun-pty stalls after a hard-killed writer (Bun launch only)

The server no longer launches under Bun (see `docs/BUN_PTY_STALL.md` for the
full investigation), so its PTY backend is node-pty and this issue does not
apply to shipped configurations. It is documented for anyone who runs
`server/index.js` under Bun manually: on Windows, if a process writing
heavily inside a terminal (hundreds of megabytes) is terminated with
`TerminateProcess` — external task-kill, or a terminal close/restart during a
flood — bun-pty's native read loop can pin one core for a backlog-proportional
time (minutes for very large floods) while the JS event loop stays idle: HTTP
stops answering, SIGINT is not delivered, and the stall drains by itself.
Reproduced without any OpenChamber code via `artifacts/term-bench/pty-variants.mjs`.
The flood backpressure and send-lag controls above bound what the server
itself buffers under either backend.
