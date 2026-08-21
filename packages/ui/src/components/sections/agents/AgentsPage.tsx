import React from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { NumberInput } from '@/components/ui/number-input';
import { Textarea } from '@/components/ui/textarea';
import { toast } from '@/components/ui';
import { useAgentsStore, getConfigDirectory, type AgentConfig, type AgentMutationResult, type AgentScope } from '@/stores/useAgentsStore';
import { useShallow } from 'zustand/react/shallow';
import { ModelSelector } from './ModelSelector';
import { useI18n } from '@/lib/i18n';
import { useOmpFeatureFlags } from '@/hooks/useOmpModelRoles';
import { parseModelIdentifier } from '@/lib/modelIdentifier';
import type { Agent } from '@/lib/opencode/wire';
import { useConfigStore } from '@/stores/useConfigStore';
import { SettingsPageLayout } from '@/components/sections/shared/SettingsPageLayout';
import {
  SettingsSection,
  SettingsFieldRow,
  SettingsStackedField,
  SettingsChipGroup,
  SETTINGS_SELECT_SIZE,
  SETTINGS_SELECT_ROW_TRIGGER_CLASS,
  SETTINGS_ICON_BUTTON_CLASS,
  SETTINGS_CUSTOM_TRIGGER_CLASS,
} from '@/components/sections/shared/SettingsSection';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Icon } from '@/components/icon/Icon';
import { AgentPermissionsEditor } from './AgentPermissionsEditor';
import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import { useOmpFeatureEnabled } from '@/hooks/useOmpFeatureEnabled';
import {
  applyTaskOverrideChanges,
  buildTaskOverrideChanges,
  emptyTaskOverrideRecords,
  parseTaskOverrideRecords,
  TASK_OVERRIDE_SETTING_KEYS,
  type TaskOverrideRecords,
} from './agentTaskOverrides';

type AgentVariantProvider = {
  id: string;
  models?: Array<{
    id?: string;
    variants?: Record<string, unknown>;
  }>;
};

const getVariantOptionsForModel = (
  providers: AgentVariantProvider[],
  modelValue: string,
): string[] => {
  const parsedModel = parseModelIdentifier(modelValue);
  if (!parsedModel) {
    return [];
  }

  const provider = providers.find((item) => item.id === parsedModel.providerId);
  const model = provider?.models?.find((item) => item.id === parsedModel.modelId);
  return model?.variants ? Object.keys(model.variants) : [];
};

/**
 * One pattern-override input (model / prewalk / advisor). Commits on Enter,
 * blur, or clear — the parent skips unchanged values.
 */
const OverridePatternField: React.FC<{
  id: string;
  label: string;
  info: string;
  placeholder: string;
  value: string;
  saving: boolean;
  onChange: (value: string) => void;
  onApply: (value: string) => void;
  clearLabel: string;
}> = ({ id, label, info, placeholder, value, saving, onChange, onApply, clearLabel }) => {
  const { t } = useI18n();
  return (
    <SettingsFieldRow settingsItem={id} label={label} info={info}>
      <Input
        value={value}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === 'Enter') onApply(value);
        }}
        onBlur={() => onApply(value)}
        placeholder={placeholder}
        disabled={saving}
        className="h-7 w-full max-w-md px-2 font-mono"
      />
      <Button
        size="sm"
        type="button"
        variant="ghost"
        disabled={saving}
        aria-label={clearLabel}
        title={clearLabel}
        onClick={() => {
          onChange('');
          onApply('');
        }}
        className={SETTINGS_ICON_BUTTON_CLASS}
      >
        <Icon name="close" className="h-3.5 w-3.5" />
      </Button>
    </SettingsFieldRow>
  );
};

/**
 * Per-agent task.* override chips (02 §5.3 per-agent override area,
 * GAP-B05). Writes go through `PUT /api/omp/settings` with the full key
 * value — the raw records are read once from the settings face so sibling
 * entries (including bundled agents absent from this page) survive every
 * write. The disabled toggle writes through immediately (agents-hub chip
 * parity); pattern inputs commit on apply. Rejected keys surface inline
 * (R9: key + reason only, never the submitted value).
 */
const AgentTaskOverridesSection: React.FC<{ agentName: string }> = ({ agentName }) => {
  const { t } = useI18n();
  const { ompSettings } = useRuntimeAPIs();
  const [records, setRecords] = React.useState<TaskOverrideRecords | null>(null);
  const [loadState, setLoadState] = React.useState<'loading' | 'ready' | 'unavailable'>('loading');
  const [rejectedKeys, setRejectedKeys] = React.useState<Array<{ key: string; reason?: string }>>([]);
  const [saving, setSaving] = React.useState(false);
  const [modelDraft, setModelDraft] = React.useState('');
  const [prewalkDraft, setPrewalkDraft] = React.useState('');
  const [advisorDraft, setAdvisorDraft] = React.useState('');

  React.useEffect(() => {
    let cancelled = false;
    setRecords(null);
    setLoadState('loading');
    setRejectedKeys([]);
    void ompSettings.getSettings({
      directory: getConfigDirectory() ?? '',
      keys: [...TASK_OVERRIDE_SETTING_KEYS],
    }).then((result) => {
      if (cancelled) return;
      if (!result.ok) {
        setLoadState('unavailable');
        return;
      }
      const parsed = parseTaskOverrideRecords(result.data);
      setRecords(parsed);
      setModelDraft(parsed.modelOverrides[agentName] ?? '');
      setPrewalkDraft(parsed.prewalk[agentName] ?? '');
      setAdvisorDraft(parsed.advisor[agentName] ?? '');
      setLoadState('ready');
    });
    return () => {
      cancelled = true;
    };
  }, [agentName, ompSettings]);

  const saveChanges = React.useCallback((changes: Record<string, unknown>) => {
    const current = records ?? emptyTaskOverrideRecords();
    if (Object.keys(changes).length === 0 || saving) return;
    setSaving(true);
    setRejectedKeys([]);
    void ompSettings.putSettings({
      directory: getConfigDirectory() ?? '',
      changes,
    }).then((result) => {
      setSaving(false);
      if (result.ok) {
        setRecords(applyTaskOverrideChanges(current, result.applied));
        return;
      }
      if (result.unavailable) {
        toast.error(t('settings.agents.page.overrides.saveFailed'));
        return;
      }
      if (result.kind === 'rejected') {
        setRejectedKeys(result.rejected);
        return;
      }
      toast.error(t(result.kind === 'quarantined'
        ? 'settings.agents.page.overrides.quarantined'
        : 'settings.agents.page.overrides.saveFailed'));
    });
  }, [ompSettings, records, saving, t]);

  if (loadState !== 'ready' || records === null) {
    return (
      <SettingsSection title={t('settings.agents.page.overrides.title')} settingsItem="agents.task-overrides">
        <p className="typography-meta text-muted-foreground">
          {loadState === 'unavailable'
            ? t('settings.agents.page.overrides.unavailable')
            : t('settings.agents.page.overrides.loading')}
        </p>
      </SettingsSection>
    );
  }

  const disabled = records.disabledAgents.includes(agentName);
  const applyPattern = (
    key: 'modelOverride' | 'prewalkOverride' | 'advisorOverride',
    value: string,
  ): void => {
    const currentEntry = key === 'modelOverride'
      ? records.modelOverrides[agentName]
      : key === 'prewalkOverride'
        ? records.prewalk[agentName]
        : records.advisor[agentName];
    if ((currentEntry ?? '') === value.trim()) return;
    saveChanges(buildTaskOverrideChanges(agentName, records, { [key]: value }));
  };

  return (
    <SettingsSection
      title={t('settings.agents.page.overrides.title')}
      info={t('settings.agents.page.overrides.hint')}
      settingsItem="agents.task-overrides"
      contentClassName="space-y-3"
    >
      <SettingsFieldRow
        settingsItem="agents.task-overrides-disabled"
        label={t('settings.agents.page.overrides.disabled.label')}
        info={t('settings.agents.page.overrides.disabled.hint')}
      >
        <Button
          variant="chip"
          size="sm"
          aria-pressed={disabled}
          disabled={saving}
          onClick={() => saveChanges(buildTaskOverrideChanges(agentName, records, { disabled: !disabled }))}
          data-testid="omp-agent-disabled-chip"
        >
          {disabled
            ? t('settings.agents.page.overrides.disabled.on')
            : t('settings.agents.page.overrides.disabled.off')}
        </Button>
      </SettingsFieldRow>

      <OverridePatternField
        id="agents.task-overrides-model"
        label={t('settings.agents.page.overrides.model.label')}
        info={t('settings.agents.page.overrides.model.hint')}
        placeholder={t('settings.agents.page.overrides.model.placeholder')}
        value={modelDraft}
        saving={saving}
        onChange={setModelDraft}
        onApply={(value) => applyPattern('modelOverride', value)}
        clearLabel={t('settings.common.actions.clear')}
      />
      <OverridePatternField
        id="agents.task-overrides-prewalk"
        label={t('settings.agents.page.overrides.prewalk.label')}
        info={t('settings.agents.page.overrides.prewalk.hint')}
        placeholder={t('settings.agents.page.overrides.prewalk.placeholder')}
        value={prewalkDraft}
        saving={saving}
        onChange={setPrewalkDraft}
        onApply={(value) => applyPattern('prewalkOverride', value)}
        clearLabel={t('settings.common.actions.clear')}
      />
      <OverridePatternField
        id="agents.task-overrides-advisor"
        label={t('settings.agents.page.overrides.advisor.label')}
        info={t('settings.agents.page.overrides.advisor.hint')}
        placeholder={t('settings.agents.page.overrides.advisor.placeholder')}
        value={advisorDraft}
        saving={saving}
        onChange={setAdvisorDraft}
        onApply={(value) => applyPattern('advisorOverride', value)}
        clearLabel={t('settings.common.actions.clear')}
      />

      {rejectedKeys.length > 0 ? (
        <div className="flex flex-col gap-1" data-testid="omp-agent-overrides-rejected" role="alert">
          {rejectedKeys.map((entry) => (
            <p key={entry.key} className="typography-meta text-status-error">
              {entry.reason
                ? t('settings.agents.page.overrides.rejectedWithReason', { key: entry.key, reason: entry.reason })
                : t('settings.agents.page.overrides.rejected', { key: entry.key })}
            </p>
          ))}
        </div>
      ) : null}
    </SettingsSection>
  );
};


export const AgentsPage: React.FC = () => {
  const { t } = useI18n();
  const ompAgentDefinitions = useOmpFeatureFlags().agentDefinitions;
  const ompSettingsEnabled = useOmpFeatureEnabled('settings.v1');
  const providers = useConfigStore((state) => state.providers) as AgentVariantProvider[];
  const {
    selectedAgentName,
    getAgentByName,
    createAgent,
    updateAgent,
    agents,
    agentDraft,
    setAgentDraft,
  } = useAgentsStore(useShallow((s) => ({
    selectedAgentName: s.selectedAgentName,
    getAgentByName: s.getAgentByName,
    createAgent: s.createAgent,
    updateAgent: s.updateAgent,
    agents: s.agents,
    agentDraft: s.agentDraft,
    setAgentDraft: s.setAgentDraft,
  })));

  const selectedAgent = selectedAgentName ? getAgentByName(selectedAgentName) : null;
  const isNewAgent = Boolean(agentDraft && agentDraft.name === selectedAgentName && !selectedAgent);

  const [draftName, setDraftName] = React.useState('');
  const [draftScope, setDraftScope] = React.useState<AgentScope>('user');
  const [description, setDescription] = React.useState('');
  const [mode, setMode] = React.useState<'primary' | 'subagent' | 'all'>('subagent');
  const [model, setModel] = React.useState('');
  const [variant, setVariant] = React.useState('');
  const [temperature, setTemperature] = React.useState<number | undefined>(undefined);
  const [topP, setTopP] = React.useState<number | undefined>(undefined);
  const [prompt, setPrompt] = React.useState('');
  const [tools, setTools] = React.useState('');
  const [isSaving, setIsSaving] = React.useState(false);
  const initialStateRef = React.useRef<{
    draftName: string;
    draftScope: AgentScope;
    description: string;
    mode: 'primary' | 'subagent' | 'all';
    model: string;
    variant: string;
    temperature: number | undefined;
    topP: number | undefined;
    prompt: string;
    tools: string;
  } | null>(null);

  const variantOptions = React.useMemo(() => getVariantOptionsForModel(providers, model), [model, providers]);
  const hasVariantOptions = variantOptions.length > 0;
  const selectedVariantValue = variant || '__default';
  const shouldUseVariantSelect = hasVariantOptions;
  const variantSelectOptions = React.useMemo(() => (
    variant && !variantOptions.includes(variant) ? [variant, ...variantOptions] : variantOptions
  ), [variant, variantOptions]);

  React.useEffect(() => {
    if (isNewAgent && agentDraft) {
      const draftNameValue = agentDraft.name || '';
      const draftScopeValue = agentDraft.scope || 'user';
      const descriptionValue = agentDraft.description || '';
      const modeValue = agentDraft.mode || 'subagent';
      const modelValue = agentDraft.model || '';
      const variantValue = agentDraft.variant || '';
      const temperatureValue = agentDraft.temperature ?? undefined;
      const topPValue = agentDraft.top_p ?? undefined;
      const promptValue = agentDraft.prompt || '';
      const toolsValue = (agentDraft.tools ?? []).join(', ');

      setDraftName(draftNameValue);
      setDraftScope(draftScopeValue);
      setDescription(descriptionValue);
      setMode(modeValue);
      setModel(modelValue);
      setVariant(variantValue);
      setTemperature(temperatureValue);
      setTopP(topPValue);
      setPrompt(promptValue);
      setTools(toolsValue);

      initialStateRef.current = {
        draftName: draftNameValue,
        draftScope: draftScopeValue,
        description: descriptionValue,
        mode: modeValue,
        model: modelValue,
        variant: variantValue,
        temperature: temperatureValue,
        topP: topPValue,
        prompt: promptValue,
        tools: toolsValue,
      };
      return;
    }

    if (selectedAgent && selectedAgentName === selectedAgent.name) {
      const descriptionValue = selectedAgent.description || '';
      const modeValue = selectedAgent.mode || 'subagent';
      const modelValue = selectedAgent.model?.providerID && selectedAgent.model?.modelID
        ? `${selectedAgent.model.providerID}/${selectedAgent.model.modelID}`
        : '';
      const promptValue = selectedAgent.prompt || '';
      const toolsValue = ((selectedAgent as Agent & { tools?: string[] }).tools ?? []).join(', ');
      const variantValue = selectedAgent.variant || '';
      const temperatureValue = selectedAgent.temperature ?? undefined;
      const topPValue = selectedAgent.topP ?? undefined;

      setDescription(descriptionValue);
      setMode(modeValue);

      setModel(modelValue);
      setVariant(variantValue);
      setTemperature(temperatureValue);
      setTopP(topPValue);
      setPrompt(promptValue);
      setTools(toolsValue);

      initialStateRef.current = {
        draftName: '',
        draftScope: 'user',
        description: descriptionValue,
        mode: modeValue,
        model: modelValue,
        variant: variantValue,
        temperature: temperatureValue,
        topP: topPValue,
        prompt: promptValue,
        tools: toolsValue,
      };
    }
  }, [agentDraft, isNewAgent, selectedAgent, selectedAgentName]);

  const isDirty = React.useMemo(() => {
    const initial = initialStateRef.current;
    if (!initial) {
      return false;
    }

    if (isNewAgent) {
      if (draftName !== initial.draftName) return true;
      if (draftScope !== initial.draftScope) return true;
    }

    if (description !== initial.description) return true;
    if (mode !== initial.mode) return true;
    if (model !== initial.model) return true;
    if (prompt !== initial.prompt) return true;
    if (tools !== initial.tools) return true;

    return false;
  }, [description, draftName, draftScope, isNewAgent, mode, model, prompt, temperature, tools, topP, variant]);

  const handleSave = async () => {
    const agentName = isNewAgent ? draftName.trim().replace(/\s+/g, '-') : selectedAgentName?.trim();

    if (!agentName) {
      toast.error(t('settings.agents.sidebar.toast.agentNameRequired'));
      return;
    }

    // Check for duplicate name when creating new agent
    if (isNewAgent && agents.some((a) => a.name === agentName)) {
      toast.error(t('settings.agents.sidebar.toast.agentExists'));
      return;
    }

    setIsSaving(true);

    try {
      const trimmedModel = model.trim();
      const trimmedVariant = variant.trim();
      const trimmedPrompt = prompt.trim();
      const parsedTools = tools.split(',').map((tool) => tool.trim()).filter(Boolean);
      const config: AgentConfig = {
        name: agentName,
        ...(description.trim() ? { description: description.trim() } : {}),
        mode,
        model: trimmedModel === '' ? null : trimmedModel,
        variant: trimmedVariant === '' ? null : trimmedVariant || undefined,
        temperature: temperature ?? null,
        top_p: topP ?? null,
        prompt: trimmedPrompt || (isNewAgent ? undefined : null),
        ...(parsedTools.length > 0 ? { tools: parsedTools } : {}),
        ...(isNewAgent && draftScope ? { scope: draftScope } : {}),
      };

      let result: AgentMutationResult;
      if (isNewAgent) {
        result = await createAgent(config);
        if (result.ok) {
          setAgentDraft(null); // Clear draft after successful creation
        }
      } else {
        result = await updateAgent(agentName, config);
      }

      if (result.ok) {
        if (result.requiresManualRestart) {
          toast.warning(t('settings.agents.page.toast.savedManualRestart'));
        } else if (result.restartDeferred) {
          toast.success(t('settings.view.pendingRestart.saved'));
        } else {
          toast.success(isNewAgent ? t('settings.agents.page.toast.created') : t('settings.agents.page.toast.updated'));
        }
      } else {
        toast.error(result.reason === 'invalid-prompt'
          ? t('settings.agents.page.toast.promptRequired')
          : result.reason === 'agent-definition-exists'
            ? t('settings.agents.sidebar.toast.agentExists')
            : isNewAgent ? t('settings.agents.page.toast.createFailed') : t('settings.agents.page.toast.updateFailed'));
      }
    } catch (error) {
      console.error('Error saving agent:', error);
      const message = error instanceof Error && error.message ? error.message : t('settings.agents.page.toast.saveUnexpectedError');
      toast.error(message);
    } finally {
      setIsSaving(false);
    }
  };

  if (!selectedAgentName) {
    return (
      <div className="flex h-full items-center justify-center">
        <div className="text-center text-muted-foreground">
          <Icon name="robot-2" className="mx-auto mb-3 h-12 w-12 opacity-50" />
          <p className="typography-body">{t('settings.agents.page.empty.title')}</p>
          <p className="typography-meta mt-1 opacity-75">{t('settings.agents.page.empty.description')}</p>
        </div>
      </div>
    );
  }

  return (
    <SettingsPageLayout
      title={isNewAgent ? t('settings.agents.page.title.new') : selectedAgentName}
      description={isNewAgent ? t('settings.agents.page.subtitle.new') : t('settings.agents.page.subtitle.edit')}
      showSaveStatus={false}
    >
      <SettingsSection
        title={t('settings.agents.page.section.identityRole')}
        divider={false}
        contentClassName="space-y-0"
      >
        {isNewAgent && (
          <SettingsFieldRow
            settingsItem="agents.name"
            label={t('settings.agents.page.field.agentName')}
          >
            <div className="flex items-center">
              <span className="typography-ui-label text-muted-foreground mr-1">@</span>
              <Input
                value={draftName}
                onChange={(e) => setDraftName(e.target.value)}
                placeholder={t('settings.agents.page.field.agentNamePlaceholder')}
                className="h-7 w-40 px-2"
              />
            </div>
            <Select value={draftScope} onValueChange={(v) => setDraftScope(v as AgentScope)}>
              <SelectTrigger size={SETTINGS_SELECT_SIZE} className="w-fit min-w-[100px]">
                <SelectValue placeholder={t('settings.agents.page.field.scopePlaceholder')} />
              </SelectTrigger>
              <SelectContent align="end">
                <SelectItem value="user">
                  <div className="flex items-center gap-2">
                    <Icon name="user-3" className="h-3.5 w-3.5" />
                    <span>{t('settings.common.scope.global')}</span>
                  </div>
                </SelectItem>
                <SelectItem value="project">
                  <div className="flex items-center gap-2">
                    <Icon name="folder" className="h-3.5 w-3.5" />
                    <span>{t('settings.common.scope.project')}</span>
                  </div>
                </SelectItem>
              </SelectContent>
            </Select>
          </SettingsFieldRow>
        )}

        <SettingsStackedField
          label={t('settings.common.field.description')}
          controlClassName="w-full max-w-none"
        >
          <Textarea
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            placeholder={t('settings.agents.page.field.descriptionPlaceholder')}
            rows={2}
            className="w-full resize-none min-h-[60px] bg-transparent"
          />
        </SettingsStackedField>

        <SettingsStackedField
          settingsItem="agents.mode"
          label={t('settings.agents.page.field.mode')}
          info={t('settings.agents.page.field.modeTooltip')}
        >
          <SettingsChipGroup
            aria-label={t('settings.agents.page.field.mode')}
            value={mode}
            onChange={setMode}
            options={ompAgentDefinitions
              ? [
                  // omp has no primary agents (02 §5.3): the build/plan
                  // concept is deleted; definitions are worker-scoped.
                  { value: 'subagent', label: t('settings.agents.page.mode.subagent') },
                  { value: 'all', label: t('settings.agents.page.mode.all') },
                ]
              : [
                  { value: 'primary', label: t('settings.agents.page.mode.primary') },
                  { value: 'subagent', label: t('settings.agents.page.mode.subagent') },
                  { value: 'all', label: t('settings.agents.page.mode.all') },
                ]}
          />
        </SettingsStackedField>

        {ompAgentDefinitions && (
          <SettingsStackedField
            label={t('settings.agents.page.field.tools')}
            info={t('settings.agents.page.field.toolsHint')}
            controlClassName="w-full max-w-none"
          >
            <Input
              value={tools}
              onChange={(e) => setTools(e.target.value)}
              placeholder={t('settings.agents.page.field.toolsPlaceholder')}
              className="h-7 w-full max-w-md px-2 font-mono"
            />
          </SettingsStackedField>
        )}
      </SettingsSection>

      {ompAgentDefinitions && ompSettingsEnabled && !isNewAgent && selectedAgent ? (
        <AgentTaskOverridesSection agentName={selectedAgent.name} />
      ) : null}

      {!ompAgentDefinitions && (
      <SettingsSection
        title={t('settings.agents.page.section.modelParameters')}
        contentClassName="space-y-3"
      >
        <SettingsFieldRow
          settingsItem="agents.model"
          label={t('settings.agents.page.field.overrideModel')}
        >
          <ModelSelector
            providerId={parseModelIdentifier(model)?.providerId ?? ''}
            modelId={parseModelIdentifier(model)?.modelId ?? ''}
            onChange={(providerId: string, modelId: string) => {
              if (providerId && modelId) {
                setModel(`${providerId}/${modelId}`);
              } else {
                setModel('');
              }
              setVariant('');
            }}
            className={SETTINGS_CUSTOM_TRIGGER_CLASS}
          />
        </SettingsFieldRow>

        <SettingsFieldRow
          settingsItem="agents.variant"
          label={t('settings.agents.page.field.variant')}
          info={(
            <div className="space-y-1">
              <p>{t('settings.agents.page.field.variantTooltip')}</p>
              <p>{t('settings.agents.page.field.variantHint')}</p>
            </div>
          )}
        >
          {shouldUseVariantSelect ? (
            <Select
              value={selectedVariantValue}
              onValueChange={(value) => setVariant(value === '__default' ? '' : value)}
            >
              <SelectTrigger size={SETTINGS_SELECT_SIZE} className={SETTINGS_SELECT_ROW_TRIGGER_CLASS}>
                <SelectValue placeholder={t('settings.agents.page.field.variantPlaceholder')}>
                  {(value) => value === '__default' ? t('chat.modelControls.default') : value}
                </SelectValue>
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="__default">{t('chat.modelControls.default')}</SelectItem>
                {variantSelectOptions.map((variantOption) => (
                  <SelectItem key={variantOption} value={variantOption}>{variantOption}</SelectItem>
                ))}
              </SelectContent>
            </Select>
          ) : (
            <>
              <Input
                value={variant}
                onChange={(event) => setVariant(event.target.value)}
                placeholder={t('settings.agents.page.field.variantPlaceholder')}
                disabled={!model && !variant}
                className="h-8 w-40 rounded-md px-3"
              />
              {variant && (
                <Button
                  size="sm"
                  type="button"
                  variant="ghost"
                  onClick={() => setVariant('')}
                  className={SETTINGS_ICON_BUTTON_CLASS}
                  aria-label={t('settings.common.actions.clear')}
                  title={t('settings.common.actions.clear')}
                >
                  <Icon name="close" className="h-3.5 w-3.5" />
                </Button>
              )}
            </>
          )}
        </SettingsFieldRow>

        <SettingsFieldRow
          settingsItem="agents.temperature"
          label={t('settings.agents.page.field.temperature')}
          info={(
            <div className="space-y-1">
              <p>{t('settings.agents.page.field.temperatureTooltip')}</p>
              <p>{t('settings.agents.page.field.temperatureRange')}</p>
            </div>
          )}
        >
          <NumberInput
            value={temperature}
            fallbackValue={0.7}
            onValueChange={setTemperature}
            onClear={() => setTemperature(undefined)}
            min={0}
            max={2}
            step={0.1}
            inputMode="decimal"
            placeholder="—"
            emptyLabel="—"
            className="w-16"
          />
          {temperature !== undefined && (
            <Button
              size="sm"
              type="button"
              variant="ghost"
              onClick={() => setTemperature(undefined)}
              className={SETTINGS_ICON_BUTTON_CLASS}
              aria-label={t('settings.agents.page.field.clearTemperatureAria')}
              title={t('settings.common.actions.clear')}
            >
              <Icon name="close" className="h-3.5 w-3.5" />
            </Button>
          )}
        </SettingsFieldRow>

        <SettingsFieldRow
          settingsItem="agents.top-p"
          label={t('settings.agents.page.field.topP')}
          info={(
            <div className="space-y-1">
              <p>{t('settings.agents.page.field.topPTooltip')}</p>
              <p>{t('settings.agents.page.field.topPRange')}</p>
            </div>
          )}
        >
          <NumberInput
            value={topP}
            fallbackValue={0.9}
            onValueChange={setTopP}
            onClear={() => setTopP(undefined)}
            min={0}
            max={1}
            step={0.1}
            inputMode="decimal"
            placeholder="—"
            emptyLabel="—"
            className="w-16"
          />
          {topP !== undefined && (
            <Button
              size="sm"
              type="button"
              variant="ghost"
              onClick={() => setTopP(undefined)}
              className={SETTINGS_ICON_BUTTON_CLASS}
              aria-label={t('settings.agents.page.field.clearTopPAria')}
              title={t('settings.common.actions.clear')}
            >
              <Icon name="close" className="h-3.5 w-3.5" />
            </Button>
          )}
        </SettingsFieldRow>
      </SettingsSection>
      )}

      <SettingsSection
        title={t('settings.agents.page.section.systemPrompt')}
        settingsItem="agents.system-prompt"
      >
        <Textarea
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
          placeholder={t('settings.agents.page.field.systemPromptPlaceholder')}
          rows={8}
          className="w-full font-mono typography-meta min-h-[120px] max-h-[60vh] bg-transparent resize-y"
        />
      </SettingsSection>

      {!isNewAgent && selectedAgent && !ompAgentDefinitions && (
        <AgentPermissionsEditor agent={selectedAgent} />
      )}

      <div className="pb-8">
        <Button
          onClick={handleSave}
          disabled={isSaving || !isDirty}
          size="xs"
          className="!font-normal"
        >
          {isSaving ? t('settings.common.actions.saving') : t('settings.common.actions.saveChanges')}
        </Button>
      </div>
    </SettingsPageLayout>
  );
};
