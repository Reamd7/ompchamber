// Local helpers for the omp agent-definition form (spec 02 §5.3).
// Model patterns / spawns / prewalk / advisor map between the CSV-ish inputs
// and the omp AgentDefinition frontmatter fields; `null` clears.


import { isAgentBuiltIn, type AgentWithExtras } from '@/stores/useAgentsStore';

/** Official oh-my-pi documentation (github.com/can1357/oh-my-pi/tree/main/docs). */
export const OMP_DOCS = {
  agentDefinitions:
    'https://github.com/can1357/oh-my-pi/blob/main/docs/task-agent-discovery.md',
  agentDefinitionShape:
    'https://github.com/can1357/oh-my-pi/blob/main/docs/task-agent-discovery.md#agent-definition-shape',
  modelPrecedence:
    'https://github.com/can1357/oh-my-pi/blob/main/docs/task-agent-discovery.md#model-and-structured-output-precedence',
  prewalk: 'https://github.com/can1357/oh-my-pi/blob/main/docs/prewalk.md',
  advisor: 'https://github.com/can1357/oh-my-pi/blob/main/docs/advisor-watchdog.md',
  agentHub: 'https://github.com/can1357/oh-my-pi/blob/main/docs/agent-hub.md',
} as const;


export type PatternMode = 'off' | 'default' | 'custom';

/** ConfiguredThinkingLevel selectors (SDK thinking.ts; server THINKING_LEVELS mirror). */
export const OMP_THINKING_LEVELS = [
  'auto', 'inherit', 'off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max',
] as const;

export const parseCsvValue = (value: string): string[] =>
  value.split(',').map((entry) => entry.trim()).filter(Boolean);

export const formatCsvValue = (values: string[] | undefined): string =>
  (values ?? []).join(', ');

export const patternModeOf = (value: boolean | string | undefined): PatternMode => {
  if (value === undefined || value === false) return 'off';
  if (value === true) return 'default';
  return 'custom';
};

export const patternValueFrom = (
  mode: PatternMode,
  pattern: string,
): boolean | string | undefined => {
  if (mode === 'off') return undefined;
  if (mode === 'default') return true;
  return pattern.trim() || undefined;
};

export const agentSourceOf = (
  agent: AgentWithExtras | null,
): 'project' | 'user' | 'bundled' | null =>
  agent?.source ?? (agent && isAgentBuiltIn(agent) ? 'bundled' : null);

/** omp form state loaded from an agent row or a new-agent draft. */
export interface OmpAgentFormState {
  description: string;
  systemPrompt: string;
  tools: string;
  modelPatterns: string;
  thinkingLevel: string;
  spawns: string;
  prewalkMode: PatternMode;
  prewalkPattern: string;
  advisorMode: PatternMode;
  advisorPattern: string;
  readSummarize: boolean;
}

export const ompFormStateFrom = (agent: AgentWithExtras | null): OmpAgentFormState => ({
  description: agent?.description ?? '',
  systemPrompt: agent?.prompt ?? '',
  tools: formatCsvValue(agent?.tools),
  modelPatterns: formatCsvValue(agent?.modelPatterns),
  thinkingLevel: agent?.thinkingLevel ?? '',
  spawns: agent?.spawns === '*' ? '*' : formatCsvValue(agent?.spawns as string[] | undefined),
  prewalkMode: patternModeOf(agent?.prewalk),
  prewalkPattern: typeof agent?.prewalk === 'string' ? agent.prewalk : '',
  advisorMode: patternModeOf(agent?.advisor),
  advisorPattern: typeof agent?.advisor === 'string' ? agent.advisor : '',
  readSummarize: agent?.readSummarize ?? false,
});

export const ompFormStatesEqual = (a: OmpAgentFormState, b: OmpAgentFormState): boolean =>
  a.description === b.description
  && a.systemPrompt === b.systemPrompt
  && a.tools === b.tools
  && a.modelPatterns === b.modelPatterns
  && a.thinkingLevel === b.thinkingLevel
  && a.spawns === b.spawns
  && a.prewalkMode === b.prewalkMode
  && a.prewalkPattern === b.prewalkPattern.trim()
  && a.advisorMode === b.advisorMode
  && a.advisorPattern === b.advisorPattern.trim()
  && a.readSummarize === b.readSummarize;
