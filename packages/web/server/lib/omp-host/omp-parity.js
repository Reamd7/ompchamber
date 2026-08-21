// omp-parity foundation: capabilities negotiation + event registry access
// (spec 05 §5.2.2/§5.2.3, master D6-R1/R2).
//
// `GET /api/omp/capabilities` is the single server-adjudicated switchboard:
// feature keys defined here gate every /api/omp domain surface, and the
// event schema version negotiates the omp event channel. Domain modules
// flip their key when their surface lands; consumers must treat a missing
// key or a 404 as "feature off" and degrade to wire-only behavior.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

const readManifest = (name) =>
  JSON.parse(fs.readFileSync(path.join(__dirname, name), 'utf8'));

/** @returns {{ eventSchema: string, events: Record<string, { durable: boolean, scope: string, snapshotEndpoints: string[], since: string, gated?: string, control?: boolean }> }} */
export const loadOmpEventRegistry = () => {
  const manifest = readManifest('omp-event-registry.json');
  return { eventSchema: manifest.eventSchema, events: manifest.events };
};

/**
 * Feature flags, server-adjudicated (master R2). A domain landing flips its
 * key here; `false` keys keep their endpoints answering explicit 501s with
 */
export const ompFeatures = () => ({
  // 05: event channel + transcript structured reads (foundation).
  events: true,
  'sessions.telemetry': true,
  // 01/06: model roles + settings proxy + per-directory keyed instances.
  'modelRoles.v1': true,
  'settings.v1': true,
  'settings.projectScopes.v1': true,
  // 02: session modes + agent-definitions + personas resources.
  'modes.v1': true,
  'agentDefinitions.v1': true,
  'personas.v1': true,
  // 03: approval + ask dialog bridge (atomic C3+C4+C5 landed; lease-driven
  // hasUI per R13).
  'dialogs.v1': true,
  // 04: local:// URI bridge + session tree + agent-runs hub.
  'uri.v1': true,
  'tree.v1': true,
  'agentRuns.v1': true,
  // 08: omp slash-command discovery for the three-layer pipeline (§5.4).
  'commands.v1': true,
  // 09: extension chrome projection — widget/status strings via the dialog
  // bridge, mirroring RpcExtensionUIRequest (chapter 09 §5.0).
  'extensionChrome.v1': true,
  // jobs: SDK AsyncJobManager only attaches to the first top-level session;
  // capability stays false until upstream injection (master R12).
  'jobs.v1': false,
  // 08: queue ack protocol needs SDK extension first (master R14).
  'queue.v1': false,
  // R15: MCP executable endpoints are out of scope this cycle; read-only +
  // disabled switches are the long-term steady state.
  'mcp.executable': false,
  'mcp.readOnly': true,
});

export const buildCapabilities = () => {
  const registry = loadOmpEventRegistry();
  return {
    version: 1,
    eventSchema: registry.eventSchema,
    features: ompFeatures(),
    minUiVersion: '0.0.0',
  };
};

export const featureEnabled = (capabilities, key) =>
  Boolean(capabilities?.features?.[key]);

/** Explicit 501 body for gated-off domain surfaces (fail loudly, R2). */
export const featureUnavailable = (key) =>
  Response.json({ error: `${key}-unavailable` }, { status: 501 });
