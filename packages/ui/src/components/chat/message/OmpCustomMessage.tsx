import React from 'react';
import type { Message, Part } from '@/lib/opencode/wire';

import { cn } from '@/lib/utils';
import { useI18n } from '@/lib/i18n';
import { Icon } from '@/components/icon/Icon';
import { useSessionUIStore } from '@/sync/session-ui-store';
import { useOmpCustomDetails } from '@/sync/useOmpSessionStore';
import type { OmpCustomDetails } from '@/sync/omp-event-reducer';
import { formatCompactTokenCount } from './turnUsage';
import { parseOmpCustomText, tierFor, type OmpCustomTier } from '../customTypeTiers';

/**
 * Tiered rendering for harness-injected `[omp:<customType>]` transcript
 * messages (spec docs/omp-parity/05 §5.8, GAP-E11 P2).
 *
 * The classification itself lives in `../customTypeTiers`; this module owns
 * the surfaces:
 * - T1 → dedicated card chrome (severity/author/jobId joined from the omp
 *   event stream by wireMessageID; plain text when no details arrived);
 * - T2 → slim collapsible divider, collapsed by default (TUI
 *   compaction-summary-message.ts affordance);
 * - T3 → nothing (defensive; the projection already filters display:false);
 * - T4 → handled by the caller: the default `[omp:<type>]` text rendering
 *   stays in place, so this module's parse returns null for it.
 */

export interface OmpCustomMessageData {
  tier: OmpCustomTier;
  customType: string;
  body: string;
  messageId: string;
  sessionId?: string;
  /** Divider metadata the projection stamps on wire info.metadata. */
  tokensBefore?: number;
  warning?: string;
  fromId?: string;
  /** modeChange dividers: the raw mode value (none/plan/goal/vibe). */
  mode?: string;
  /** modelChange dividers: the switch's role attribution tag. */
  role?: string;
  /** modelChange dividers: the switch landed on a retry-fallback model. */
  fallback?: boolean;
}

const readTextOfPart = (part: Part): string => {
  const text = (part as { text?: unknown }).text;
  return typeof text === 'string' ? text : '';
};

/**
 * Parse one wire message into tiered custom data. Only assistant messages
 * whose visible surface is exactly one `[omp:<type>]` text part qualify —
 * anything else (mixed parts, user roles, T4/unknown types) returns null and
 * keeps the default MessageBody rendering.
 */
// eslint-disable-next-line react-refresh/only-export-components -- pure parse helper colocated with the tiered renderer
export function parseOmpCustomMessage(info: Message, parts: Part[]): OmpCustomMessageData | null {
  if (parts.length !== 1 || parts[0]?.type !== 'text') return null;
  const parsed = parseOmpCustomText(readTextOfPart(parts[0]));
  if (!parsed) return null;
  const tier = tierFor(parsed.customType);
  if (tier === 'T4') return null;

  const metadata = (info as { metadata?: Record<string, unknown> }).metadata;
  const tokensBefore = typeof metadata?.tokensBefore === 'number' ? metadata.tokensBefore : undefined;
  const fromId = typeof metadata?.fromId === 'string' ? metadata.fromId : undefined;
  const warning = typeof metadata?.warning === 'string' && metadata.warning.length > 0 ? metadata.warning : undefined;
  const mode = typeof metadata?.mode === 'string' && metadata.mode.length > 0 ? metadata.mode : undefined;
  const role = typeof metadata?.role === 'string' && metadata.role.length > 0 ? metadata.role : undefined;
  const fallback = metadata?.fallback === true ? true : undefined;

  return {
    tier,
    customType: parsed.customType,
    body: parsed.body,
    messageId: info.id,
    sessionId: info.sessionID,
    tokensBefore,
    warning,
    mode,
    role,
    fallback
  };
}

// ---------------------------------------------------------------------------
// T1 — visible cards
// ---------------------------------------------------------------------------

export interface OmpCardChrome {
  typeLabel: string;
  severity?: string;
  author?: string;
  jobId?: string;
  /** Secondary muted chip (launch-completion daemon name/state). */
  meta?: string;
}

const ADVISOR_SEVERITY_RANK: Record<string, number> = {
  nit: 1,
  concern: 2,
  blocker: 3
};

/**
 * Derive the card chrome fields (05 §5.8.2: advisor severity rail, irc
 * author, async-result jobId) from the joined `omp.custom.appended` details.
 * Unknown detail shapes yield nothing — the card then degrades to its
 * plain-text body.
 */
// eslint-disable-next-line react-refresh/only-export-components -- pure chrome derivation colocated with the card
export function cardChromeFrom(customType: string, details: OmpCustomDetails | null | undefined): OmpCardChrome {
  const chrome: OmpCardChrome = { typeLabel: customType };
  const detailsRecord = details?.details as Record<string, unknown> | undefined;

  if (customType === 'advisor') {
    const notes = Array.isArray(detailsRecord?.notes) ? detailsRecord.notes : [];
    let worstSeverity: string | undefined;
    let firstAdvisor: string | undefined;
    for (const note of notes) {
      const entry = note as { severity?: unknown; advisor?: unknown };
      const severity = typeof entry.severity === 'string' ? entry.severity : undefined;
      const advisor = typeof entry.advisor === 'string' ? entry.advisor : undefined;
      if (advisor && !firstAdvisor) firstAdvisor = advisor;
      if (severity && (ADVISOR_SEVERITY_RANK[severity] ?? 0) > (ADVISOR_SEVERITY_RANK[worstSeverity ?? ''] ?? 0)) {
        worstSeverity = severity;
      }
    }
    if (worstSeverity) chrome.severity = worstSeverity;
    if (firstAdvisor) chrome.author = firstAdvisor;
    return chrome;
  }

  if (customType.startsWith('irc:')) {
    const from = typeof detailsRecord?.from === 'string' ? detailsRecord.from : undefined;
    chrome.author = from ?? details?.attribution;
    return chrome;
  }

  if (customType === 'async-result') {
    const jobs = Array.isArray(detailsRecord?.jobs) ? (detailsRecord.jobs as unknown[]) : [];
    const firstJob = jobs[0] as { jobId?: unknown } | undefined;
    const batchedJobId = firstJob && typeof firstJob.jobId === 'string' ? firstJob.jobId : undefined;
    const singleJobId = typeof detailsRecord?.jobId === 'string' ? detailsRecord.jobId : undefined;
    const jobId = singleJobId ?? batchedJobId;
    if (jobId) chrome.jobId = jobId;
    return chrome;
  }

  if (customType === 'launch-completion') {
    // SDK buildLaunchCompletionBatchMessage: details.daemons[] each carry
    // { name, state, exitCode? } for one terminal supervised process.
    const daemons = Array.isArray(detailsRecord?.daemons) ? detailsRecord.daemons : [];
    const first = daemons[0] as { name?: unknown; state?: unknown } | undefined;
    const name = first && typeof first.name === 'string' ? first.name : undefined;
    const state = first && typeof first.state === 'string' ? first.state : undefined;
    // One batch message can carry several terminal processes; the chip names
    // the first and counts the rest (their details ride the body text).
    if (name) chrome.meta = `${state ? `${name} (${state})` : name}${daemons.length > 1 ? ` +${daemons.length - 1}` : ''}`;
    return chrome;
  }

  if (customType === 'collab-prompt') {
    // SDK CollabPromptDetails { from }: the TUI renders «from › as the author.
    const from = typeof detailsRecord?.from === 'string' && detailsRecord.from.trim() ? detailsRecord.from.trim() : undefined;
    if (from) chrome.author = from;
    return chrome;
  }
  return chrome;
}

/**
 * Structured detail lines rendered above the body (TUI card parity:
 * SkillMessageComponent name/path/args, LateDiagnosticsMessageComponent
 * per-file rows, background-tan-dispatch jobId/work). Returns plain literal
 * values (paths, ids, summaries); unknown shapes yield no lines.
 */
// eslint-disable-next-line react-refresh/only-export-components -- pure detail derivation colocated with the card
export function cardDetailLinesFrom(customType: string, details: OmpCustomDetails | null | undefined): { lines: string[]; moreCount: number } {
  const detailsRecord = details?.details as Record<string, unknown> | undefined;
  if (!detailsRecord) return { lines: [], moreCount: 0 };
  if (customType === 'skill-prompt') {
    const lines: string[] = [];
    if (typeof detailsRecord.path === 'string' && detailsRecord.path) {
      lines.push(detailsRecord.path);
    }
    if (typeof detailsRecord.args === 'string' && detailsRecord.args) {
      lines.push(detailsRecord.args);
    }
    return { lines, moreCount: 0 };
  }
  if (customType === 'background-tan-dispatch') {
    const lines: string[] = [];
    if (typeof detailsRecord.jobId === 'string' && detailsRecord.jobId) {
      lines.push(detailsRecord.jobId);
    }
    if (typeof detailsRecord.work === 'string' && detailsRecord.work) {
      lines.push(detailsRecord.work);
    }
    return { lines, moreCount: 0 };
  }
  if (customType === 'lsp-late-diagnostic') {
    // SDK LateDiagnosticsFile { path?, summary?, errored?, messages? }.
    const files = Array.isArray(detailsRecord.files) ? (detailsRecord.files as unknown[]) : [];
    const LSP_MAX_FILES = 5;
    const lines: string[] = [];
    let moreCount = 0;
    for (const entry of files) {
      if (typeof entry !== 'object' || entry === null) continue;
      const file = entry as {
        path?: unknown;
        summary?: unknown;
        errored?: unknown;
      };
      const path = typeof file.path === 'string' ? file.path : undefined;
      const summary = typeof file.summary === 'string' ? file.summary : undefined;
      const line = path && summary ? `${path} (${summary})` : (path ?? summary);
      if (!line) continue;
      if (lines.length < LSP_MAX_FILES) {
        lines.push(file.errored === true ? `${line} [error]` : line);
      } else {
        moreCount += 1;
      }
    }
    return { lines, moreCount };
  }
  return { lines: [], moreCount: 0 };
}

const severityClassName = (severity: string): string => {
  if (severity === 'blocker') return 'text-[color:var(--status-error)]';
  if (severity === 'concern') return 'text-[color:var(--status-warning)]';
  return 'text-muted-foreground/70';
};

/** Presentational T1 card — chrome badges plus the (label-stripped) body. */
export const OmpCustomCardView: React.FC<{
  chrome: OmpCardChrome;
  body: string;
  detailLines?: string[];
  moreLine?: string;
}> = ({ chrome, body, detailLines, moreLine }) => (
  <div className="my-2 max-w-full rounded-lg border border-border/40 bg-muted/20 px-3 py-2" data-omp-custom-card={chrome.typeLabel}>
    <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-xs text-muted-foreground/80">
      {chrome.meta ? <span className="truncate opacity-80">· {chrome.meta}</span> : null}
      {chrome.author ? <span className="truncate">· {chrome.author}</span> : null}
      {chrome.jobId ? <span className="truncate font-mono">· {chrome.jobId}</span> : null}
      {chrome.severity ? <span className={cn('rounded border border-border/40 px-1', severityClassName(chrome.severity))}>{chrome.severity}</span> : null}
    </div>
    {detailLines && detailLines.length > 0 ? (
      <ul className="mt-1 space-y-0.5 font-mono text-[11px] break-all text-muted-foreground">
        {detailLines.map((line, index) => (
          <li key={index}>{line}</li>
        ))}
        {moreLine ? <li className="opacity-80">{moreLine}</li> : null}
      </ul>
    ) : null}
    {body.length > 0 ? <p className="mt-1 break-words whitespace-pre-wrap text-sm text-foreground/80">{body}</p> : null}
  </div>
);

/**
 * T1 card with the store join: structured details arrive via
 * `omp.custom.appended` / the custom-messages cold read, keyed by
 * wireMessageID. Without details the card degrades to plain text (05 §5.8.2).
 */
const OmpCustomCard: React.FC<{ data: OmpCustomMessageData }> = ({ data }) => {
  const { t } = useI18n();
  const getDirectoryForSession = useSessionUIStore((state) => state.getDirectoryForSession);
  const directory = data.sessionId ? getDirectoryForSession(data.sessionId) : null;
  const details = useOmpCustomDetails(directory ?? '', data.messageId);

  if (!details) {
    return (
      <p className="my-2 break-words whitespace-pre-wrap text-sm text-muted-foreground/80" data-omp-custom-plain={data.customType}>
        {data.body}
      </p>
    );
  }
  const detail = cardDetailLinesFrom(data.customType, details);
  return <OmpCustomCardView chrome={cardChromeFrom(data.customType, details)} body={data.body} detailLines={detail.lines} moreLine={detail.moreCount > 0 ? t('chat.omp.customCard.moreEntries', { count: detail.moreCount }) : undefined} />;
};

// ---------------------------------------------------------------------------
// T2 — collapsible summary dividers
// ---------------------------------------------------------------------------

interface SummaryDividerViewProps {
  expanded: boolean;
  onToggle: () => void;
  label: string;
  summary: string;
  tokensBefore?: number;
  warning?: string;
}

/**
 * Slim history-collapse divider (TUI SummaryDividerComponent: `── label ──`,
 * collapsed by default; expanding reveals the summary detail box). Purely
 * presentational so both disclosure states are renderable in tests.
 */
export const SummaryDividerView: React.FC<SummaryDividerViewProps> = ({ expanded, onToggle, label, summary, tokensBefore, warning }) => {
  const { t } = useI18n();
  const tokensLabel =
    tokensBefore !== undefined
      ? t('chat.chat.ompDivider.fromTokens', {
          tokens: formatCompactTokenCount(tokensBefore)
        })
      : undefined;
  return (
    <div data-omp-summary-divider={label}>
      <div className="my-2 flex items-center gap-2">
        <span className="h-px flex-1 bg-border/50" aria-hidden="true" />
        <button
          type="button"
          role="button"
          aria-expanded={expanded}
          aria-label={expanded ? t('chat.chat.ompDivider.collapse') : t('chat.chat.ompDivider.expand')}
          className="flex min-w-0 items-center gap-1.5 rounded px-1 py-0.5 text-xs text-muted-foreground/80 transition-colors hover:text-foreground focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--interactive-focus-ring)]"
          onClick={(event) => {
            event.preventDefault();
            onToggle();
          }}
        >
          <Icon name={expanded ? 'arrow-down-s' : 'arrow-right-s'} className="h-3.5 w-3.5 shrink-0" />
          <span className="truncate">{label}</span>
          {tokensLabel ? <span className="shrink-0 opacity-70">{tokensLabel}</span> : null}
          {warning ? (
            <span className="shrink-0 text-[color:var(--status-warning)]" title={warning} aria-label={warning}>
              ⚠
            </span>
          ) : null}
        </button>
        <span className="h-px flex-1 bg-border/50" aria-hidden="true" />
      </div>
      {expanded ? (
        <div className="mt-2 max-w-full rounded-lg border border-border/40 bg-muted/20 px-3 py-2.5">
          {tokensLabel ? <p className="mb-1 text-xs font-medium text-muted-foreground">{tokensLabel}</p> : null}
          {warning ? <p className="mb-1 break-words text-xs text-[color:var(--status-warning)]">⚠ {warning}</p> : null}
          <p className="break-words whitespace-pre-wrap text-sm text-foreground/80">{summary}</p>
        </div>
      ) : null}
    </div>
  );
};
/** T2 divider with local disclosure state — collapsed until clicked. */
const OmpSummaryDivider: React.FC<{ data: OmpCustomMessageData }> = ({ data }) => {
  const { t } = useI18n();
  const [expanded, setExpanded] = React.useState(false);
  const label = data.customType === 'compactionSummary' ? t('chat.chat.ompDivider.compacted') : data.customType === 'branchSummary' ? t('chat.chat.ompDivider.branchSummary') : data.customType === 'handoff' ? t('chat.chat.ompDivider.handoff') : data.customType === 'modelChange' ? t('chat.chat.ompDivider.modelChange') : data.customType === 'modeChange' ? t('chat.chat.ompDivider.modeChange') : data.customType;
  // The mode value is protocol vocabulary; render it with the same localized
  // labels the mode chip uses when it is a known mode, raw otherwise.
  const modeLabelKey = data.mode === 'none' ? 'chat.modeSelector.label.default' : data.mode === 'plan' ? 'chat.modeSelector.label.plan' : data.mode === 'goal' ? 'chat.modeSelector.label.goal' : data.mode === 'vibe' ? 'chat.modeSelector.label.vibe' : null;
  const summary = data.customType === 'modeChange' && modeLabelKey !== null ? t(modeLabelKey) : data.customType === 'modelChange' ? [data.body, data.role ? t('chat.chat.ompDivider.modelChange.role', { role: data.role }) : null, data.fallback ? t('chat.chat.ompDivider.modelChange.fallback') : null].filter((line): line is string => line !== null).join('\n') : data.body;
  return <SummaryDividerView expanded={expanded} onToggle={() => setExpanded((value) => !value)} label={label} summary={summary} tokensBefore={data.tokensBefore} warning={data.warning} />;
};

// ---------------------------------------------------------------------------
// T5 — collapsed harness-injected notes
// ---------------------------------------------------------------------------

interface CollapsedNoteViewProps {
  expanded: boolean;
  onToggle: () => void;
  label: string;
  body: string;
}

/**
 * Collapsed harness note (TUI CollapsedSyntheticMessageComponent): one
 * summary line; the body — potentially hundreds of KiB — mounts only after
 * expanding. Purely presentational so both disclosure states render in tests.
 */
export const CollapsedNoteView: React.FC<CollapsedNoteViewProps> = ({ expanded, onToggle, label, body }) => {
  const { t } = useI18n();
  const summary = body.split('\n')[0] ?? '';
  return (
    <div data-omp-collapsed-note={label}>
      <button
        type="button"
        aria-expanded={expanded}
        aria-label={expanded ? t('chat.chat.ompDivider.collapse') : t('chat.chat.ompDivider.expand')}
        className="flex w-full min-w-0 items-center gap-1.5 rounded px-1 py-0.5 text-xs text-muted-foreground/80 transition-colors hover:text-foreground focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--interactive-focus-ring)]"
        onClick={(event) => {
          event.preventDefault();
          onToggle();
        }}
      >
        <Icon name={expanded ? 'arrow-down-s' : 'arrow-right-s'} className="h-3.5 w-3.5 shrink-0" />
        <span className="shrink-0 font-medium">{label}</span>
        {summary ? <span className="min-w-0 truncate opacity-80">{summary}</span> : null}
      </button>
      {expanded ? <pre className="mt-1 max-h-64 overflow-auto rounded-lg border border-border/40 bg-muted/20 p-2 text-xs break-words whitespace-pre-wrap text-foreground/80">{body}</pre> : null}
    </div>
  );
};

/** T5 note with local disclosure state — collapsed until clicked. */
const OmpDeveloperNote: React.FC<{ data: OmpCustomMessageData }> = ({ data }) => {
  const { t } = useI18n();
  const [expanded, setExpanded] = React.useState(false);
  return <CollapsedNoteView expanded={expanded} onToggle={() => setExpanded((value) => !value)} label={t('chat.chat.ompDivider.developerNote')} body={data.body} />;
};

export const OmpCustomMessage: React.FC<{ data: OmpCustomMessageData }> = ({ data }) => {
  if (data.tier === 'T3') return null;
  if (data.tier === 'T1') return <OmpCustomCard data={data} />;
  if (data.tier === 'T5') return <OmpDeveloperNote data={data} />;
  return <OmpSummaryDivider data={data} />;
};
