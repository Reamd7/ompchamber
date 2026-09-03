// Domain module: omp-parity chapter 08 §5.4 (slash command pipeline), server
// side.
//
// GET /omp/commands (public path /api/omp/commands, R3) is the omp command
// discovery surface feeding the UI's three-layer slash pipeline:
//   Tier A (`tier: 'client-builtin'`) — omp built-in semantic commands, names
//     reserved by the engine (the full BUILTIN_SLASH_COMMANDS_INTERNAL
//     registry, including TUI-only handlers, because collision resolution is
//     about NAMES — /debug /compact /review must never silently resolve to an
//     OMPChamber layer).
//   Tier B (`tier: 'engine'`) — commands the engine expands itself when the
//     text reaches a materialized session: file markdown commands (loaded
//     from the directory), skills, and (on live sessions) extension/custom
//     TS commands.
//
// Discovery is headless: buildAvailableSlashCommands runs against a synthetic
// AvailableCommandsSession (skills via discoverSkills — same precedent as the
// wire /skill route — and file commands from the requested directory's cwd).
// Extension/custom TS commands need a live session's extension runner and are
// therefore absent here; the engine still expands them if sent, and the UI
// must not treat this list as exhaustive for those sources.
//
// Capability `commands.v1` gates the endpoint (master R2): a missing/false
// key answers an explicit 501 so the client falls back to its legacy
// two-source resolution (skills store + OC commands store, 08 §5.4).
//
// SELF-CONTAINED BY CONTRACT: no engine.js/endpoints.js imports; the
// coordinator mounts registerCommandsDomainRoutes(route, { features }).

import { discoverSkills } from '@oh-my-pi/pi-coding-agent';
import {
  buildAvailableSlashCommands,
} from '@oh-my-pi/pi-coding-agent/slash-commands/available-commands';
import {
  BUILTIN_SLASH_COMMANDS_INTERNAL,
} from '@oh-my-pi/pi-coding-agent/slash-commands/builtin-registry';
import type {
  AvailableCommandsSession,
  InternalAvailableSlashCommand,
} from '@oh-my-pi/pi-coding-agent/slash-commands/available-commands';
import type { Skill } from '@oh-my-pi/pi-coding-agent/extensibility/skills';
import { featureUnavailable, ompFeatures } from './omp-parity.ts';

/** Response.json's own parameter contract — platform-owned passthrough. */
type ResponseJsonData = Parameters<typeof Response.json>[0];
const json = (data: ResponseJsonData, init?: ResponseInit) => Response.json(data, init);

/** Response tiers (08 §5.4: 'client-builtin' = Tier A, 'engine' = Tier B). */
export const OMP_COMMAND_TIERS = Object.freeze(['client-builtin', 'engine'] as const);

/** Engine-expandable sources; everything else (builtin) is client-side. */
const tierForSource = (source?: string): OmpCommandTier => (source === 'builtin' ? 'client-builtin' : 'engine');

export type OmpCommandTier = (typeof OMP_COMMAND_TIERS)[number];

export interface ProjectableOmpCommand {
  name?: string;
  aliases?: string[];
  description?: string;
  input?: { hint?: string };
  acpInputHint?: string;
  inlineHint?: string;
  source?: string;
}

export interface OmpCommandRecord {
  name: string;
  description?: string;
  tier: OmpCommandTier;
  source: string;
  aliases?: string[];
  argumentHint?: string;
}

/**
 * Project one InternalAvailableSlashCommand / SlashCommandSpec into the wire
 * record. `description` and the argument template stay optional — the only
 * guaranteed fields are name/tier/source.
 */
export const projectOmpCommand = (internal: ProjectableOmpCommand | null): OmpCommandRecord | null => {
  if (!internal || typeof internal.name !== 'string' || !internal.name) return null;
  const hint = internal.input?.hint ?? internal.acpInputHint ?? internal.inlineHint;
  return {
    name: internal.name,
    ...(typeof internal.description === 'string' && internal.description
      ? { description: internal.description }
      : {}),
    tier: tierForSource(internal.source),
    source: internal.source ?? 'engine',
    ...(Array.isArray(internal.aliases) && internal.aliases.length > 0
      ? { aliases: internal.aliases.filter((a) => typeof a === 'string' && a) }
      : {}),
    ...(typeof hint === 'string' && hint ? { argumentHint: hint } : {}),
  };
};

/** Tier A rows: the engine's reserved built-in command names. */
export const builtinOmpCommands = (): OmpCommandRecord[] => {
  const rows: OmpCommandRecord[] = [];
  for (const spec of BUILTIN_SLASH_COMMANDS_INTERNAL) {
    // BUILTIN_SLASH_COMMANDS_INTERNAL rows carry the raw description;
    // acpDescription overrides it where defined (available-commands.ts:49).
    const projected = projectOmpCommand({
      name: spec.name,
      aliases: spec.aliases,
      description: spec.acpDescription ?? spec.description,
      input: spec.acpInputHint ?? spec.inlineHint ? { hint: spec.acpInputHint ?? spec.inlineHint } : undefined,
      source: 'builtin',
    });
    if (projected) rows.push(projected);
  }
  return rows;
};

/** Production Tier B loader (headless AvailableCommandsSession). */
const defaultLoadAvailable = (session: AvailableCommandsSession) => buildAvailableSlashCommands(session);

/** Production skills loader — same discovery the wire /skill route uses. */
const defaultLoadSkills = async (directory: string): Promise<readonly Skill[]> => {
  const { skills } = await discoverSkills(directory);
  return skills ?? [];
};

export type OmpLiveCommandsLoader = (
  directory: string,
) => Promise<readonly ProjectableOmpCommand[] | null | undefined>;

export interface ListOmpCommandsInput {
  directory: string;
  loadAvailable?: (
    session: AvailableCommandsSession,
  ) => Promise<readonly InternalAvailableSlashCommand[] | null | undefined>;
  loadSkills?: (directory: string) => Promise<readonly Skill[]>;
  loadLiveCommands?: OmpLiveCommandsLoader | null;
}

/**
 * Aggregate the omp command list for one directory. Builtins always lead
 * (name reservation); engine commands are appended name-deduped, mirroring
 * buildAvailableSlashCommands' first-seen-wins semantics. A discovery failure
 * degrades to the builtin-only list — never to an empty success, and never to
 * a 500 that would take the whole autocomplete merge down.
 *
 * @param {{ directory: string, loadAvailable?: (session: object) => Promise<Array<object>>, loadSkills?: (directory: string) => Promise<Array<object>> }} input
 */
export const listOmpCommands = async ({
  directory,
  loadAvailable = defaultLoadAvailable,
  loadSkills = defaultLoadSkills,
  loadLiveCommands = null,
}: ListOmpCommandsInput): Promise<OmpCommandRecord[]> => {
  const commands: OmpCommandRecord[] = [];
  const seen = new Set<string>();
  const append = (record: OmpCommandRecord | null) => {
    if (!record || seen.has(record.name)) return;
    seen.add(record.name);
    commands.push(record);
  };
  for (const record of builtinOmpCommands()) append(record);
  try {
    const skills = await loadSkills(directory);
    const available = await loadAvailable({
      customCommands: [],
      skills: Array.isArray(skills) ? skills : [],
      // Default per settings-schema.ts:4786-4790 (skills.enableSkillCommands).
      skillsSettings: { enableSkillCommands: true },
      setSlashCommands: () => {},
      sessionManager: { getCwd: () => directory },
    });
    for (const internal of available ?? []) {
      if (internal?.source === 'builtin') continue; // already covered above
      append(projectOmpCommand(internal));
    }
  } catch {
    // Degraded, not authoritative-empty: the builtin half is still real.
  }
  // Live-session extension commands (09 §5.4 discovery gap): the headless
  // synthetic session above has no extension runner, so commands registered
  // by user extensions (pi.registerCommand) only exist on materialized
  // sessions. A degraded/absent live source appends nothing — the builtin
  // + headless halves stay authoritative.
  if (typeof loadLiveCommands === 'function') {
    try {
      for (const live of (await loadLiveCommands(directory)) ?? []) {
        append(projectOmpCommand({ ...live, source: live?.source ?? 'extension' }));
      }
    } catch {
      // Live source unavailable — same degradation contract as above.
    }
  }
  return commands;
};

export interface CommandsRouteMountOptions {
  features?: Record<string, boolean>;
  list?: typeof listOmpCommands;
  liveCommandsFor?: OmpLiveCommandsLoader | null;
}

type CommandsRouteMount = (
  method: string,
  pattern: string,
  handler: (request: Request) => Response | Promise<Response>,
) => void;

/**
 * Mount the /omp routes owned by this domain. Capability-gated per master R2:
 * `commands.v1` off ⇒ explicit 501 (clients fall back to the legacy
 * two-source resolution, never a silent empty list).
 * @param {(method: string, pattern: string, handler: Function) => void} route
 * @param {{ features?: Record<string, boolean>, list?: typeof listOmpCommands, liveCommandsFor?: ((directory: string) => Promise<Array<object>>) | null }} [options]
 */
export function registerCommandsDomainRoutes(
  route: CommandsRouteMount,
  { features = ompFeatures(), list = listOmpCommands, liveCommandsFor = null }: CommandsRouteMountOptions = {},
): void {
  route('GET', '/omp/commands', async (request) => {
    if (features?.['commands.v1'] !== true) return featureUnavailable('commands.v1');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory') ?? process.cwd();
    return json(await list({
      directory,
      ...(typeof liveCommandsFor === 'function' ? { loadLiveCommands: liveCommandsFor } : {}),
    }));
  });
}
