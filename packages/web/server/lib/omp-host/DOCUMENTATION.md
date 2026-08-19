# omp host

The managed engine behind OpenChamber's OpenCode-compatible API surface.

## What this module is

`host.js` is an HTTP+SSE server (run under Bun) that embeds
`@oh-my-pi/pi-coding-agent` sessions and serves the wire contract the shared
UI consumes. It replaces the previously managed `opencode serve` child
process: same launch shape (`serve --hostname --port`), same readiness stdout
line (`opencode server listening on <url>`), same Basic-auth model driven by
`OPENCODE_SERVER_PASSWORD`, same health endpoint (`GET /global/health`).

The wire contract itself is owned by OpenChamber and lives in
`packages/ui/src/lib/opencode/wire` (vendored generated client + types). The
route table implemented here is exactly the subset of that contract the UI,
sync engine, and web server call; everything else answers 404.

## Modules

- `host.js` — entrypoint, route dispatch, Basic auth, SSE writer, shutdown.
- `engine.js` — `OmpHostEngine`: model/auth boot, session materialization
  (cold reads via `SessionManager` transcript projection, live turns via
  `createAgentSession` + event pump), custom-agent storage, session
  operations (create/list/update/delete/fork/revert/unrevert/summarize/abort/
  move), idle-session sweeping.
- `projection.js` — pure omp→wire translation. Deterministic message/part ids
  derived from omp message identity so live streaming and cold re-projection
  agree (the UI message loader merges both).
- `events.js` — wire event bus: monotonic ids, 2048-event replay ring for
  Last-Event-ID resume, per-directory scoping for `/event`.
- `registry.js` — per-project sidecar metadata omp does not persist
  (archived time, parentID, title overrides, model/agent selection, revert
  pointers, custom agents), written atomically.
- `endpoints.js` — route handlers.

## Invariants

- Deterministic ids: wire message ids are `msg_<role><base36 timestamp><digest>`
  derived from omp message identity — never from persistence entry ids.
- Failure honesty: unimplemented OpenCode features return explicit 501s
  (session sharing, provider OAuth via API, MCP OAuth bridging, session
  shell), not empty successes.
- Every emitted event carries the session's directory so `/event?directory=`
  scoping and the web server's hub routing stay correct.
- Sessions are persisted by omp's `SessionManager` under the cwd-derived
  session directory; OpenChamber metadata lives only in the sidecar registry.

## Launch resolution (see `../opencode/omp-host-launch.js`)

1. `OPENCHAMBER_OMP_HOST_BINARY` — a compiled self-contained host
   (`bun build --compile`, staged by the desktop packaging).
2. Bundled `resources/omp-host/omp-host(.exe)`.
3. Bun runtime (`OPENCHAMBER_OMP_HOST_RUNTIME` → current process → PATH)
   launching `host.js` from source — the development path.

## Known engine gaps (explicit, not silent)

Permission/question protocols answer authoritatively empty until the omp
approval/ask bridge lands; providers expose `ModelRegistry.getAvailable()`
models only; `PUT /auth` defers to omp's credential store.
