import React from 'react';
import { z } from 'zod';
import { runtimeFetch, type RuntimeFetchOptions } from '@/lib/runtime-fetch';
import { OMP_ENDPOINTS, parseOmpAgentRunTranscript } from '@/lib/api/omp';
import { useI18n } from '@/lib/i18n';
import { Icon } from '@/components/icon/Icon';
import { openOmpArtifact } from '@/lib/omp/openArtifact';

/**
 * Agent run transcript (ch 14 §4.3): the read-only drill-in surface over
 * GET /omp/agent-runs/{sessionID}/{agentId}/transcript — one subagent run's
 * wire-projected conversation as a lightweight chronological list. On-demand
 * fetch (open = fresh data); a run without a readable transcript (historical
 * disk row) or an older server renders the empty state. The durable task
 * artifact (`agent://` output, when the run wrote one) opens from the header.
 */

/** One projected message narrowed to what this list renders. */
type RunMessageRow = {
  role: string;
  text: string;
  tools: Array<{ name: string; status: string }>;
};

const ROLE_ICON = {
  user: 'user-3',
  assistant: 'ai-agent',
  system: 'terminal-box',
} as const;

const roleIcon = (role: string): 'user-3' | 'ai-agent' | 'terminal-box' =>
  role === 'user' || role === 'assistant' || role === 'system' ? ROLE_ICON[role] : 'terminal-box';

const RunPartSchema = z.looseObject({
  type: z.string(),
  text: z.string().optional(),
  tool: z.string().optional(),
  state: z.looseObject({ status: z.string().optional() }).optional(),
});

const RunMessageSchema = z.object({
  info: z.object({ role: z.string() }),
  parts: z.array(RunPartSchema).optional(),
});

const RunMessagesSchema = z.array(RunMessageSchema);
/** Map one schema-parsed conversation message to its row shape. */
const toRunMessageRow = (message: z.infer<typeof RunMessageSchema>): RunMessageRow => {
  const row: RunMessageRow = { role: message.info.role, text: '', tools: [] };
  const texts: string[] = [];
  for (const part of message.parts ?? []) {
    if (part.type === 'text' && part.text?.trim()) {
      texts.push(part.text.trim());
    } else if (part.type === 'tool' && part.tool) {
      row.tools.push({ name: part.tool, status: part.state?.status ?? '' });
    }
  }
  row.text = texts.join('\n\n');
  return row;
};

export const AgentRunTab: React.FC<{
  sessionID: string;
  agentId: string;
  directory: string;
}> = ({ sessionID, agentId, directory }) => {
  const { t } = useI18n();
  const [messages, setMessages] = React.useState<RunMessageRow[] | null>(null);
  const [outputPath, setOutputPath] = React.useState<string | null>(null);
  const [artifactFailed, setArtifactFailed] = React.useState(false);
  const [loading, setLoading] = React.useState(false);

  React.useEffect(() => {
    let cancelled = false;
    setLoading(true);
    const query = `?directory=${encodeURIComponent(directory)}`;
    // SAFETY: RuntimeFetchOptions is runtimeFetch's RequestInit superset; the
    // literal carries method/headers only.
    void runtimeFetch(`${OMP_ENDPOINTS.agentRunTranscript(sessionID, agentId)}${query}`, {
      method: 'GET',
      headers: { Accept: 'application/json' },
    } as RuntimeFetchOptions)
      .then(async (response) => {
        if (cancelled) return;
        if (!response.ok) {
          setMessages(null);
          setOutputPath(null);
          return;
        }
        const transcript = parseOmpAgentRunTranscript(await response.json());
        const parsedMessages = RunMessagesSchema.safeParse(transcript?.messages);
        setMessages(parsedMessages.success ? parsedMessages.data.map(toRunMessageRow) : null);
        setOutputPath(transcript?.outputPath ?? null);
      })
      .catch(() => {
        if (!cancelled) {
          setMessages(null);
          setOutputPath(null);
        }
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionID, agentId, directory]);

  const openArtifact = React.useCallback(() => {
    if (!outputPath) return;
    setArtifactFailed(false);
    void openOmpArtifact(outputPath, sessionID, directory).then((ok) => {
      if (!ok) setArtifactFailed(true);
    });
  }, [outputPath, sessionID, directory]);

  if (loading) {
    return <div className="p-4 typography-meta text-muted-foreground">{t('contextPanel.agentRun.loading')}</div>;
  }
  if (!messages || messages.length === 0) {
    return <div className="p-4 typography-meta text-muted-foreground">{t('contextPanel.agentRun.empty')}</div>;
  }

  return (
    <div className="flex h-full flex-col overflow-y-auto p-3">
      <div className="flex flex-col gap-3">
        {outputPath ? (
          <div className="flex items-center gap-2 pb-1">
            <button
              type="button"
              onClick={openArtifact}
              aria-label={t('contextPanel.agentRun.openArtifact')}
              title={t('contextPanel.agentRun.openArtifact')}
              className="inline-flex items-center gap-1.5 rounded-md px-1.5 py-0.5 typography-meta text-primary transition-colors hover:text-primary/80"
            >
              <Icon name="attachment-2" className="h-3 w-3 flex-shrink-0" />
            </button>
            {artifactFailed ? (
              <span className="typography-micro text-[var(--status-error)]">{t('contextPanel.agentRun.artifactFailed')}</span>
            ) : null}
          </div>
        ) : null}
        {messages.map((row, index) => (
          <div key={index} className="flex flex-col gap-1">
            <div className="flex items-center gap-1.5">
              <Icon
                name={roleIcon(row.role)}
                className="h-3 w-3 flex-shrink-0 text-muted-foreground"
              />
              <span className="typography-micro uppercase tracking-wide text-muted-foreground/70">{row.role}</span>
            </div>
            {row.text ? (
              <div className="whitespace-pre-wrap break-words text-[13px] text-foreground/90">{row.text}</div>
            ) : null}
            {row.tools.length > 0 ? (
              <div className="flex flex-col gap-0.5 pl-4">
                {row.tools.map((tool, toolIndex) => (
                  <div key={toolIndex} className="flex items-center gap-1.5 typography-meta text-muted-foreground/80">
                    <span
                      className={
                        tool.status === 'error'
                          ? 'text-[var(--status-error)]'
                          : tool.status === 'completed'
                            ? 'text-muted-foreground/60'
                            : 'text-[var(--status-info)]'
                      }
                    >
                      ●
                    </span>
                    <span className="truncate">{tool.name}</span>
                    {tool.status ? <span className="text-muted-foreground/50">{tool.status}</span> : null}
                  </div>
                ))}
              </div>
            ) : null}
          </div>
        ))}
      </div>
    </div>
  );
};
