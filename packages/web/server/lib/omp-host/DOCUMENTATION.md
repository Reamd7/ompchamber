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

- `host.js` — entrypoint, route dispatch, Basic auth, SSE writer, shutdown.
- `engine.js` — `OmpHostEngine`: model/auth boot, session materialization
  (cold reads via `SessionManager` transcript projection, live turns via
  `createAgentSession` + event pump), session operations, idle sweeping.
  Materialization injects the per-directory keyed `Settings` instance
  (`options.settings`, spec 06 §5.1/master R6), the lease-driven `hasUI`
  snapshot (R13), and session-pinned `localProtocolOptions` (R7/R8), and
  retains the private `AgentRegistry` + `CreateAgentSessionResult` handle
  for the agent-runs aggregator and `setToolUIContext`. The first live lease
  initializes the SDK extension runner through its canonical
  `initializeExtensions(..., { mode: 'json', uiContext })` helper and updates
  the tool context; a lease present before first materialization is applied
  directly before publishing the host session. This keeps both custom-command
  dialogs and tool approvals interactive on the first turn.
- `projection.js` — pure omp→wire translation with deterministic ids;
  projects dividers (`compactionSummary`/`branchSummary`), execution roles
  (`bashExecution`/`pythonExecution`), `fileMention`, and streaming partial
  tool output (`toolPartial`, never terminal).
- `events.js` — `RingEventBus` (durable/volatile replay split) with the wire
  `WireEventBus` on top, plus `OmpEventBus`: the single omp-native channel
  (envelope `{id,type,directory,sessionID?,schemaVersion,createdAt,payload}`,
  process-global monotonic ids, 512-durable ring, gap/restart detection
  feeding `omp.stream.resync`).
- `registry.js` — per-project sidecar metadata, written atomically.
- `endpoints.js` — wire route handlers + the `/omp/*` parity group:
  capabilities, the omp SSE channel, transcript structured reads
  (custom-messages/telemetry/entries), `GET /agent-dir` (the Node web server
  cannot import this SDK — it resolves the profile-scoped omp agent dir
  here), and mounts for the domain modules below.
- `omp-parity.js` — `ompFeatures()` capability table (the server-adjudicated
  switchboard, master R2) + registry access.
- `domain-models.js` (specs 01/06) — per-directory keyed Settings store
  (`cloneForCwd` derivation, boot instance as global-write executor),
  `/omp/models` role payload, `/omp/settings` proxy with credential
  sanitization (R9) and modelRoles-only project writes (R6), legacy
  defaultModel detect/import.
- `domain-dialogs.js` (spec 03) — `UiLeaseTable` (per-session UI attachment
  leases: the ONLY `hasUI` authority, R13), `PendingDialogRegistry`
  (atomic respond/abort, presented-ack `T_answer` + `T_present` TTLs, orphan
  settle on lease loss, R11 shutdown settle-all), the WebUIContext bridge,
  and the always-allow write-first transaction.
- `domain-modes.js` (spec 02) — mode tracker (mode_change persistence +
  cold recovery), plan review bridge (`setPlanProposalHandler`), persona
  resolution, agent-definitions/personas CRUD, `planOptionsFor` (the
  `PlanYolo {target,thinkingLevel?}` shape — the old
  `{autoApproveOnResolve}` literal was the P0 defect).
- `domain-uri.js` (spec 04) — local:// bridge (session-pinned, zero global
  mutation), URI token service (no absolute sourcePath echo, R7), session
  tree, `AgentRunsAggregator` (250ms coalesced `omp.agents.updated`),
  parked/historical split, jobs 501 steady state (R12).
- `domain-commands.js` (spec 08 §5.4) — `GET /api/omp/commands?directory=`:
  omp slash-command discovery for the UI's three-layer pipeline. Tier A rows
  (`client-builtin`) are the full reserved builtin registry; Tier B rows
  (`engine`) come from a headless `buildAvailableSlashCommands` session
  (skills via `discoverSkills` + file commands from the directory; extension/
  custom TS commands need a live session and stay absent). Gated by
  `commands.v1`; discovery failure degrades to the builtin-only list.
- `domain-plugins.js` (spec 09/OMP plugin parity) — `/api/omp/plugins` reads the SDK `PluginManager` and marketplace registries, exposes only sanitized metadata (never install paths), and `/api/omp/plugins/extensions/*` reads the profile-scoped `.omp/agent/extensions` files. Mutations return deferred-restart outcomes because extension discovery is session-start state.
- `event-dispositions.json` / `omp-event-registry.json` /
  `omp-bootstrap-matrix.json` — machine-checked contracts.
  `scripts/check-event-coverage.mjs` (repo root, `bun run check:events`)
  enforces: SDK union == manifest keys, engine switch covers every member,
  omp names registered, durable events' snapshot endpoints covered by the
  bootstrap matrix, and no parallel `openchamber:omp` channels (R1).

## Invariants

- Deterministic ids: wire message ids are `msg_<role letter><base36 timestamp><digest>`
  (`a` assistant, `u` user, `c` custom transcript notes) derived from omp
  message identity — never from persistence entry ids. Live streaming and
  cold re-projection must agree for the same message: streaming derives an
  assistant id at `message_start` (empty content, start timestamp) while the
  persisted message finalizes both, and prompted user messages must echo the
  client's `messageID` for optimistic reconciliation. The engine bridges
  these through an in-memory `wireIdOverrides` map (cold id → live/echo id,
  captured from the user `message_start` and assistant `message_end` events)
  that cold projections resolve via `wireIdFor`; without it every re-fetch
  projected a second, different id for the same message and the UI rendered
  the prompt or reply twice. Overrides live only for the engine process
  lifetime — an engine restart forces a reconnect whose authoritative refetch
  replaces client state wholesale, so stale live ids cannot linger.
  Transcript `custom_message` entries (advisor nudges, todo reminders, late
  diagnostics) project as assistant-side notes prefixed `[omp:<type>]` with
  `synthetic: true` parts; entries marked `display: false` are dropped.
- Failure honesty: unimplemented OpenCode features return explicit 501s
  (session sharing, provider OAuth via API, MCP OAuth bridging, session
  shell), not empty successes.
- Every emitted event carries the session's directory so `/event?directory=`
  scoping and the web server's hub routing stay correct.
- Message history paging follows the OpenCode cursor contract:
  `GET /session/:id/message` caps the newest tail at `limit`, treats `before`
  as an exclusive message-id boundary, and answers with an `x-next-cursor`
  header (oldest id of the page) only while older messages remain. An unknown
  `before` id returns an empty page so clients stop instead of looping. The
  shared UI's session-message loader drives its "load older" flow entirely
  from this contract.
- Prompt bodies follow the wire contract's `parts` array
  (`SessionPromptAsyncData`): text parts join into the prompt text, `file`
  parts decode from `data:` URLs into image inputs, agent mentions append
  when absent from the text, and `messageID` is echoed as the projected user
  message id so the client's optimistic message reconciles in place during
  the live turn. The legacy `{ prompt: { text, files } }` body is still
  accepted by the synchronous `/message` route for the gitApi consumer.
  Dropping `parts` persisted user messages with no parts, which the UI hides
  after reload.
- Session titles follow the TUI's semantics: every user prompt attempts
  `maybeStartTitleGeneration` at submission time while the turn runs, and pi
  itself skips the call once the session is named or retries on later
  messages after a failed/low-signal attempt. Generated names flow back
  through `onSessionNameChanged` into the sidecar registry and a
  `session.updated` event. Never gate this on registry bookkeeping like
  `timeCreated` — `createSession` writes it at creation, so web-created
  sessions would never attempt a title (that is exactly the regression this
  rule came from).
- Sessions are persisted by omp's `SessionManager` under the cwd-derived
  session directory; OpenChamber metadata lives only in the sidecar registry.
- SDK usage follows the TUI's semantics wherever both exist; when omp-host
  behavior diverges from the TUI, the TUI is wrong-by-default and the change
  needs an explicit reason. Currently aligned: submission dispatch always
  passes a `streamingBehavior` (wire `delivery: "queue"` → `followUp`,
  everything else → `steer`, the TUI's Enter-while-streaming semantic) so a
  live turn steers or queues the prompt instead of rejecting it with
  `AgentBusyError`; routing through `prompt()` rather than `steer()` also
  keeps `/`-extension commands working mid-turn. Session construction never
  pins a fallback model — with no persisted selector the SDK resolves the
  settings default (`defaultModel`/`defaultProvider`) exactly like the TUI
  (pinning `getAvailable()[0]` once overrode the user's default with the
  alphabetically-first model). Each embedded session passes its own private
  `AgentRegistry` because the SDK's global registry admits a single "Main"
  agent per process generation while omp-host keeps several top-level
  sessions live concurrently.

## Launch resolution (see `../opencode/omp-host-launch.js`)

1. `OPENCHAMBER_OMP_HOST_BINARY` — a compiled self-contained host
   (`bun build --compile`, staged by the desktop packaging).
2. Bundled `resources/omp-host/omp-host(.exe)`.
3. Bun runtime (`OPENCHAMBER_OMP_HOST_RUNTIME` → current process → PATH)
   launching `host.js` from source — the development path.

## Known engine gaps (explicit, not silent)

Permission/question protocols answer authoritatively empty until their UI
 consumers switch to the dialogs bridge (07's observation window); providers
 expose `ModelRegistry.getAvailable()` models only; `PUT /auth` defers to
 omp's credential store; jobs answer a structured 501 with `ownerSessionID`
 until the SDK exposes manager injection (R12); the queue ack protocol stays
 off until the SDK grows stable enqueue ids (R14); MCP stays read-only
 (R15); agent:// history:// artifact:// host-side resolution stays
 capability-off until upstream per-resolve artifactsDirs (R7).

## Test topology

`bun test server/lib/omp-host/` (Bun) owns this module — the tests mock the
SDK by specifier including its deep `config/...` paths, and the real SDK
graph executes Bun globals at module top level, so vitest (Node) excludes
this directory and the web package's `test` script chains both runners.
