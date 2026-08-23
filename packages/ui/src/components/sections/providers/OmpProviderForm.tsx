/**
 * OmpProviderForm — create/edit one engine custom provider
 * (`providers.v1` GUI CRUD over the omp engine's models.yml).
 *
 * Layout follows the Cherry Studio / LobeChat provider-editor pattern: a
 * wide centered column (max-w-3xl) with a Connection section (label-above,
 * full-width fields) and a Models section (single-line rows with column
 * header, not a six-column table), plus a sticky footer with Save/Cancel.
 * Persistence goes through `ompProviders.putProvider` / `fetchModels`
 * (comment-preserving server write + engine registry refresh). Field-merge
 * contract: keys this form does not show (thinking blocks, compat, cost, …)
 * survive a save server-side; an empty API key keeps the existing one.
 *
 * The form reports draft dirtiness upward (`onDirtyChange`) so the host page
 * can guard the back/cancel affordance against silent data loss.
 */
import React from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Icon } from '@/components/icon/Icon';
import { toast } from '@/components/ui';
import {
  SettingsSection,
  SettingsStackedField,
  SETTINGS_FIELDS_STACK_CLASS,
} from '@/components/sections/shared/SettingsSection';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
/** Official provider/model configuration docs (models.yml reference). */
const PROVIDER_DOCS_URL = 'https://omp.sh/docs/providers';
import { useI18n } from '@/lib/i18n';
import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import {
  OMP_PROVIDER_API_VALUES,
  type OmpFileProvider,
  type OmpModelEffort,
  type OmpProviderApiValue,
  type OmpProviderModelInput,
} from '@/lib/api/omp';
import { OmpModelDialog, type OmpModelDraft } from './OmpModelDialog';

const PROVIDER_ID_PATTERN = /^[a-z0-9][a-z0-9-_]*$/;

interface ModelRowState {
  key: string;
  id: string;
  name: string;
  reasoning: boolean;
  contextWindow: string;
  maxTokens: string;
  /** Draft thinking config from the model dialog. */
  thinkingEfforts: OmpModelEffort[];
  thinkingDefault: string;
  /** True once the dialog saved a thinking change — only then does the form
   * submit a `thinking` key (untouched hand-authored blocks survive). */
  thinkingTouched: boolean;
  /** File has some thinking block (badge), incl. legacy shapes the dialog
   * cannot round-trip; untouched rows keep it byte-for-byte. */
  hasThinkingOriginal: boolean;
  /** Capabilities / cost / advanced dialog fields. */
  imageInput: boolean;
  supportsTools: '' | 'on' | 'off';
  costInput: string;
  costOutput: string;
  costCacheRead: string;
  costCacheWrite: string;
  baseUrl: string;
  api: string;
  omitMaxOutputTokens: boolean;
  contextPromotionTarget: string;
  compactionModel: string;
  /** True once the dialog saved this row (marks extended keys as managed). */
  dialogTouched: boolean;
  hasImageOriginal: boolean;
  costOriginal: boolean;
}

let rowKeyCounter = 0;
const nextRowKey = () => `omp-model-row-${rowKeyCounter++}`;

const emptyModelDraft = (): OmpModelDraft => ({
  id: '',
  name: '',
  reasoning: false,
  contextWindow: '',
  maxTokens: '',
  thinkingEfforts: [],
  thinkingDefault: '',
  imageInput: false,
  supportsTools: '',
  costInput: '',
  costOutput: '',
  costCacheRead: '',
  costCacheWrite: '',
  baseUrl: '',
  api: '',
  omitMaxOutputTokens: false,
  contextPromotionTarget: '',
  compactionModel: '',
});

const emptyModelRow = (): ModelRowState => ({
  key: nextRowKey(),
  id: '',
  name: '',
  reasoning: false,
  contextWindow: '',
  maxTokens: '',
  thinkingEfforts: [],
  thinkingDefault: '',
  thinkingTouched: false,
  hasThinkingOriginal: false,
  imageInput: false,
  supportsTools: '',
  costInput: '',
  costOutput: '',
  costCacheRead: '',
  costCacheWrite: '',
  baseUrl: '',
  api: '',
  omitMaxOutputTokens: false,
  contextPromotionTarget: '',
  compactionModel: '',
  dialogTouched: false,
  hasImageOriginal: false,
  costOriginal: false,
});

const rowsFromProvider = (provider: OmpFileProvider): ModelRowState[] =>
  (provider.models.length > 0 ? provider.models : [null]).map((model) => ({
    key: nextRowKey(),
    id: model?.id ?? '',
    name: model?.name ?? '',
    reasoning: model?.reasoning ?? false,
    contextWindow: model?.contextWindow !== undefined ? String(model.contextWindow) : '',
    maxTokens: model?.maxTokens !== undefined ? String(model.maxTokens) : '',
    thinkingEfforts: (model?.thinking?.efforts ?? []).filter((e): e is OmpModelEffort =>
      (['minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const).includes(e as OmpModelEffort)),
    thinkingDefault: model?.thinking?.defaultLevel ?? '',
    thinkingTouched: false,
    hasThinkingOriginal: model?.hasThinking === true || (model?.thinking?.efforts?.length ?? 0) > 0,
    imageInput: (model?.input ?? []).includes('image'),
    supportsTools: model?.supportsTools === undefined ? '' : model.supportsTools ? 'on' : 'off',
    costInput: model?.cost !== undefined ? String(model.cost.input) : '',
    costOutput: model?.cost !== undefined ? String(model.cost.output) : '',
    costCacheRead: model?.cost !== undefined ? String(model.cost.cacheRead) : '',
    costCacheWrite: model?.cost !== undefined ? String(model.cost.cacheWrite) : '',
    baseUrl: model?.baseUrl ?? '',
    api: model?.api ?? '',
    omitMaxOutputTokens: false,
    contextPromotionTarget: model?.contextPromotionTarget ?? '',
    compactionModel: model?.compactionModel ?? '',
    dialogTouched: false,
    hasImageOriginal: (model?.input ?? []).includes('image'),
    costOriginal: model?.cost !== undefined,
  }));

const rowsEqual = (a: ModelRowState[], b: ModelRowState[]) =>
  a.length === b.length && a.every((row, index) => (
    row.id === b[index].id
    && row.name === b[index].name
    && row.reasoning === b[index].reasoning
    && row.contextWindow === b[index].contextWindow
    && row.maxTokens === b[index].maxTokens
    && row.thinkingTouched === b[index].thinkingTouched
    && row.thinkingDefault === b[index].thinkingDefault
    && row.thinkingEfforts.join(',') === b[index].thinkingEfforts.join(',')
    && row.imageInput === b[index].imageInput
    && row.supportsTools === b[index].supportsTools
    && row.costInput === b[index].costInput
    && row.costOutput === b[index].costOutput
    && row.costCacheRead === b[index].costCacheRead
    && row.costCacheWrite === b[index].costCacheWrite
    && row.baseUrl === b[index].baseUrl
    && row.api === b[index].api
    && row.omitMaxOutputTokens === b[index].omitMaxOutputTokens
    && row.contextPromotionTarget === b[index].contextPromotionTarget
    && row.compactionModel === b[index].compactionModel
    && row.dialogTouched === b[index].dialogTouched
  ));

export interface OmpProviderFormProps {
  /** Edit target; omit for create. */
  provider?: OmpFileProvider;
  /** Ids already taken (create path must not collide). */
  existingProviderIds: ReadonlySet<string>;
  onDone: (providerId: string) => void;
  /** Ask before losing a modified draft. */
  onRequestClose: () => void;
  onDirtyChange?: (dirty: boolean) => void;
}

export const OmpProviderForm: React.FC<OmpProviderFormProps> = ({
  provider,
  existingProviderIds,
  onDone,
  onRequestClose,
  onDirtyChange,
}) => {
  const { t } = useI18n();
  const { ompProviders } = useRuntimeAPIs();
  const isEdit = provider !== undefined;

  const [providerId, setProviderId] = React.useState(provider?.id ?? '');
  const [baseUrl, setBaseUrl] = React.useState(provider?.baseUrl ?? '');
  const [api, setApi] = React.useState<OmpProviderApiValue>(
    (provider?.api as OmpProviderApiValue | undefined) ?? 'openai-responses',
  );
  const [apiKey, setApiKey] = React.useState('');
  const [showApiKey, setShowApiKey] = React.useState(false);
  const [authHeader, setAuthHeader] = React.useState(provider?.authHeader ?? false);
  const [rows, setRows] = React.useState<ModelRowState[]>(() => rowsFromProvider(provider ?? { id: '', source: 'file', hasApiKey: false, models: [] }));
  const [fieldErrors, setFieldErrors] = React.useState<{ id?: string; baseUrl?: string; apiKey?: string; modelId?: string; form?: string }>({});
  const [saving, setSaving] = React.useState(false);
  const [fetching, setFetching] = React.useState(false);
  const [modelDialog, setModelDialog] = React.useState<{ rowKey: string | null; draft: OmpModelDraft | null }>({ rowKey: null, draft: null });

  const initial = React.useRef({
    providerId: provider?.id ?? '',
    baseUrl: provider?.baseUrl ?? '',
    api: (provider?.api as OmpProviderApiValue | undefined) ?? ('openai-responses' as OmpProviderApiValue),
    apiKey: '',
    authHeader: provider?.authHeader ?? false,
    rows: rowsFromProvider(provider ?? { id: '', source: 'file', hasApiKey: false, models: [] }),
  });

  const isDirty = !saving && (
    providerId !== initial.current.providerId
    || baseUrl !== initial.current.baseUrl
    || api !== initial.current.api
    || apiKey.trim() !== ''
    || authHeader !== initial.current.authHeader
    || !rowsEqual(rows, initial.current.rows)
  );
  React.useEffect(() => {
    onDirtyChange?.(isDirty);
  }, [isDirty, onDirtyChange]);

  const handleSubmit = async () => {
    const errors: typeof fieldErrors = {};
    const id = providerId.trim();
    if (!PROVIDER_ID_PATTERN.test(id)) errors.id = t('settings.providers.omp.error.id');
    else if (!isEdit && existingProviderIds.has(id)) errors.id = t('settings.providers.omp.error.exists', { provider: id });
    if (!/^https?:\/\//.test(baseUrl.trim())) errors.baseUrl = t('settings.providers.omp.error.baseUrl');
    if (!isEdit && !apiKey.trim()) errors.apiKey = t('settings.providers.omp.error.apiKeyRequired');
    const models: OmpProviderModelInput[] = [];
    for (const row of rows) {
      if (!row.id.trim()) {
        errors.modelId = t('settings.providers.omp.error.modelId');
        break;
      }
    }
    if (errors.id || errors.baseUrl || errors.apiKey || errors.modelId) {
      setFieldErrors(errors);
      return;
    }
    for (const row of rows) {
      models.push({
        id: row.id.trim(),
        ...(row.name.trim() ? { name: row.name.trim() } : {}),
        ...(row.reasoning ? { reasoning: true } : {}),
        ...(row.contextWindow.trim() ? { contextWindow: Number(row.contextWindow) } : {}),
        ...(row.maxTokens.trim() ? { maxTokens: Number(row.maxTokens) } : {}),
        // Only the dialog's own saves send a thinking key; untouched
        // hand-authored blocks survive the merge server-side.
        ...(row.thinkingTouched
          ? { thinking: row.thinkingEfforts.length > 0
              ? { mode: 'effort' as const, efforts: row.thinkingEfforts, ...(row.thinkingDefault ? { defaultLevel: row.thinkingDefault } : {}) }
              : null }
          : {}),
        // Extended managed keys — only after the dialog saved this row, so
        // hand-authored config survives untouched rows byte-for-byte.
        ...(row.dialogTouched ? {
          input: row.imageInput ? ['text', 'image'] : ['text'],
          ...(row.supportsTools === '' ? { supportsTools: null } : { supportsTools: row.supportsTools === 'on' }),
          ...(row.costInput.trim() || row.costOutput.trim() || row.costCacheRead.trim() || row.costCacheWrite.trim()
            ? { cost: {
                input: Number(row.costInput.trim() || 0),
                output: Number(row.costOutput.trim() || 0),
                cacheRead: Number(row.costCacheRead.trim() || 0),
                cacheWrite: Number(row.costCacheWrite.trim() || 0),
              } }
            : { cost: null }),
          ...(row.baseUrl.trim() ? { baseUrl: row.baseUrl.trim() } : { baseUrl: null }),
          ...(row.api ? { api: row.api } : {}),
          ...(row.omitMaxOutputTokens ? { omitMaxOutputTokens: true } : {}),
          ...(row.contextPromotionTarget.trim() ? { contextPromotionTarget: row.contextPromotionTarget.trim() } : { contextPromotionTarget: null }),
          ...(row.compactionModel.trim() ? { compactionModel: row.compactionModel.trim() } : { compactionModel: null }),
        } : {}),
      });
    }

    setSaving(true);
    setFieldErrors({});
    const result = await ompProviders.putProvider({
      provider: {
        id,
        baseUrl: baseUrl.trim(),
        api,
        // Empty key on edit = keep the stored one (server merge contract).
        ...(apiKey.trim() ? { apiKey: apiKey.trim() } : {}),
        ...(isEdit ? { authHeader } : (authHeader ? { authHeader } : {})),
        models,
      },
    });
    setSaving(false);
    if (result.ok) {
      toast.success(t('settings.providers.omp.toast.saved'));
      onDone(result.provider.id);
      return;
    }
    if (result.unavailable) {
      setFieldErrors({ form: t('settings.providers.omp.error.unavailable') });
      return;
    }
    setFieldErrors({ form: result.message ?? t('settings.providers.omp.toast.saveFailed') });
  };

  const handleFetchModels = async () => {
    const targetId = (providerId.trim() || provider?.id || '').trim();
    if (!targetId) {
      setFieldErrors((current) => ({ ...current, id: t('settings.providers.omp.error.id') }));
      return;
    }
    setFetching(true);
    setFieldErrors({});
    const result = await ompProviders.fetchModels(targetId, {
      ...(baseUrl.trim() ? { baseUrl: baseUrl.trim() } : {}),
      ...(apiKey.trim() ? { apiKey: apiKey.trim() } : {}),
    });
    setFetching(false);
    if (!result.ok) {
      setFieldErrors({
        form: result.unavailable
          ? t('settings.providers.omp.error.unavailable')
          : (result.message ?? t('settings.providers.omp.error.fetchFailed')),
      });
      return;
    }
    const existingIds = new Set(rows.map((row) => row.id.trim()).filter(Boolean));
    const fetched = result.models.filter((model) => !existingIds.has(model));
    if (fetched.length === 0) {
      toast.info(t('settings.providers.omp.toast.fetchNoNew', { count: result.models.length }));
      return;
    }
    setRows((current) => [
      ...current.filter((row) => row.id.trim().length > 0),
      ...fetched.map((modelId) => ({ ...emptyModelRow(), id: modelId, name: modelId })),
    ]);
    toast.success(t('settings.providers.omp.toast.fetched', { count: fetched.length }));
  };

  const requiredMark = <span aria-hidden="true" className="text-[var(--status-error)]">*</span>;

  return (
    <form
      className="mx-auto flex w-full min-w-0 max-w-3xl flex-col gap-5 px-6 py-6 @3xl:px-12"
      onSubmit={(event) => {
        event.preventDefault();
        void handleSubmit();
      }}
    >
      <SettingsSection title={t('settings.providers.omp.section.connection')} divider={false}>
        <div className={SETTINGS_FIELDS_STACK_CLASS}>
          {isEdit ? null : (
            <SettingsStackedField label={<span>{t('settings.providers.omp.field.id')} {requiredMark}</span>} info={t('settings.providers.omp.field.id.info')} infoDocsUrl={PROVIDER_DOCS_URL} controlClassName="max-w-md">
              <Input
                value={providerId}
                onChange={(event) => { setProviderId(event.target.value); setFieldErrors((c) => ({ ...c, id: undefined })); }}
                placeholder={t('settings.providers.omp.field.id.placeholder')}
                className="w-full font-mono text-xs"
                aria-label={t('settings.providers.omp.field.id')}
                aria-required={!isEdit}
                aria-invalid={fieldErrors.id !== undefined}
                spellCheck={false}
                autoFocus
              />
              {fieldErrors.id ? <p className="typography-micro text-[var(--status-error)]" role="alert">{fieldErrors.id}</p> : null}
            </SettingsStackedField>
          )}

          <SettingsStackedField label={<span>{t('settings.providers.omp.field.baseUrl')} {requiredMark}</span>} controlClassName="max-w-none">
            <Input
              value={baseUrl}
              onChange={(event) => { setBaseUrl(event.target.value); setFieldErrors((c) => ({ ...c, baseUrl: undefined })); }}
              placeholder={t('settings.providers.omp.field.baseUrl.placeholder')}
              className="w-full font-mono text-xs"
              aria-label={t('settings.providers.omp.field.baseUrl')}
              aria-required="true"
              aria-invalid={fieldErrors.baseUrl !== undefined}
              spellCheck={false}
            />
            {fieldErrors.baseUrl ? <p className="typography-micro text-[var(--status-error)]" role="alert">{fieldErrors.baseUrl}</p> : null}
          </SettingsStackedField>

          <SettingsStackedField label={t('settings.providers.omp.field.api')} info={t('settings.providers.omp.field.api.info')} infoDocsUrl={PROVIDER_DOCS_URL} controlClassName="max-w-md">
            <Select value={api} onValueChange={(value) => setApi(value as OmpProviderApiValue)}>
              <SelectTrigger className="w-full max-w-sm" size="settings" aria-label={t('settings.providers.omp.field.api')}>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {OMP_PROVIDER_API_VALUES.map((value) => (
                  <SelectItem key={value} value={value}>{value}</SelectItem>
                ))}
              </SelectContent>
            </Select>
          </SettingsStackedField>

          <SettingsStackedField
            label={<span>{isEdit && provider.hasApiKey ? t('settings.providers.omp.field.apiKey') : <>{t('settings.providers.omp.field.apiKey')} {requiredMark}</>}</span>}
            info={t('settings.providers.omp.field.apiKey.info')} infoDocsUrl={PROVIDER_DOCS_URL}
            controlClassName="max-w-none"
          >
            <div className="flex items-center gap-1.5">
              <Input
                type={showApiKey ? 'text' : 'password'}
                value={apiKey}
                onChange={(event) => { setApiKey(event.target.value); setFieldErrors((c) => ({ ...c, apiKey: undefined })); }}
                placeholder={isEdit && provider.hasApiKey
                  ? t('settings.providers.omp.field.apiKey.keepPlaceholder')
                  : t('settings.providers.omp.field.apiKey.placeholder')}
                className="w-full font-mono text-xs"
                aria-label={t('settings.providers.omp.field.apiKey')}
                aria-required={!isEdit}
                aria-invalid={fieldErrors.apiKey !== undefined}
                spellCheck={false}
              />
              <Button
                type="button"
                variant="ghost"
                size="icon"
                className="size-8 shrink-0 text-muted-foreground hover:text-foreground"
                onClick={() => setShowApiKey((prev) => !prev)}
                aria-label={showApiKey ? t('settings.providers.omp.actions.hideKey') : t('settings.providers.omp.actions.showKey')}
              >
                <Icon name={showApiKey ? 'eye-off' : 'eye'} className="size-4" />
              </Button>
            </div>
            {fieldErrors.apiKey ? <p className="typography-micro text-[var(--status-error)]" role="alert">{fieldErrors.apiKey}</p> : null}
            <p className="typography-micro text-[var(--status-warning)]">{t('settings.providers.omp.field.apiKey.plaintextWarning')}</p>
          </SettingsStackedField>

          <label className="flex cursor-pointer items-center gap-2 typography-ui-label text-foreground">
            <input
              type="checkbox"
              checked={authHeader}
              onChange={(event) => setAuthHeader(event.target.checked)}
              className="size-4 accent-[var(--interactive-accent)]"
              aria-label={t('settings.providers.omp.field.authHeader')}
            />
            {t('settings.providers.omp.field.authHeader')}
            <span className="typography-micro text-muted-foreground">{t('settings.providers.omp.field.authHeaderShort.info')}</span>
          </label>
        </div>
      </SettingsSection>

      <SettingsSection
        title={t('settings.providers.omp.models.title')}
        titleAccessory={<span className="typography-micro font-normal text-muted-foreground">({rows.filter((r) => r.id.trim()).length})</span>}
        headerAction={(
          <div className="flex items-center gap-1">
            {isEdit || providerId.trim() ? (
              <Button type="button" variant="outline" size="xs" className="!font-normal" onClick={() => { void handleFetchModels(); }} disabled={fetching}>
                <Icon name={fetching ? 'loader-4' : 'refresh'} className={fetching ? 'mr-1 size-3.5 animate-spin' : 'mr-1 size-3.5'} />
                {fetching ? t('settings.providers.omp.actions.fetching') : t('settings.providers.omp.actions.fetch')}
              </Button>
            ) : null}
            <Button type="button" variant="outline" size="xs" className="!font-normal" onClick={() => setModelDialog({ rowKey: null, draft: emptyModelDraft() })}>
              <Icon name="add" className="mr-1 size-3.5" />
              {t('settings.providers.omp.models.add')}
            </Button>
          </div>
        )}
      >
        {rows.length === 0 ? (
          <p className="typography-meta py-4 text-center text-muted-foreground">{t('settings.providers.omp.models.empty')}</p>
        ) : (
          <div className="divide-y divide-[var(--surface-subtle)]">
            {rows.map((row) => (
              <div key={row.key} className="group flex min-h-11 items-center gap-2.5 py-1.5">
                <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                  <span className="truncate font-mono text-xs font-medium text-foreground" title={row.id}>{row.id}</span>
                  <span className="truncate typography-micro text-muted-foreground" title={row.name}>{row.name || '—'}</span>
                </div>
                <div className="flex shrink-0 items-center gap-1.5">
                  {row.reasoning ? (
                    <span className="typography-micro rounded border border-border/50 px-1.5 py-0.5 text-muted-foreground" title={t('settings.providers.omp.models.reasoning')}>{t('settings.providers.omp.models.reasoningShort')}</span>
                  ) : null}
                  {(row.thinkingTouched ? row.thinkingEfforts.length > 0 : row.hasThinkingOriginal) ? (
                    <span className="typography-micro rounded border border-border/50 px-1.5 py-0.5 text-muted-foreground" title={row.thinkingEfforts.join(', ')}>{t('settings.providers.omp.models.thinkingBadge')}</span>
                  ) : null}
                  {(row.dialogTouched ? row.imageInput : row.hasImageOriginal) ? (
                    <span className="typography-micro rounded border border-border/50 px-1.5 py-0.5 text-muted-foreground" title={t('settings.providers.omp.modelDialog.imageInput')}>{t('settings.providers.omp.models.imageBadge')}</span>
                  ) : null}
                  {(row.dialogTouched ? row.supportsTools === 'off' : row.supportsTools === 'off') ? (
                    <span className="typography-micro rounded border border-border/50 px-1.5 py-0.5 text-muted-foreground" title={t('settings.providers.omp.modelDialog.toolsXml')}>{t('settings.providers.omp.models.toolsBadge')}</span>
                  ) : null}
                  {(row.dialogTouched ? (row.costInput.trim() !== '' && Number(row.costInput) > 0) : row.costOriginal) ? (
                    <span className="typography-micro text-muted-foreground" title={t('settings.providers.omp.modelDialog.cost.info')}>
                      ${row.costInput.trim() || '?'}/M
                    </span>
                  ) : null}
                  {row.contextWindow.trim() ? (
                    <span className="typography-micro text-muted-foreground" title={t('settings.providers.omp.models.context')}>{row.contextWindow.trim()}</span>
                  ) : null}
                  {row.maxTokens.trim() ? (
                    <span className="typography-micro text-muted-foreground" title={t('settings.providers.omp.models.maxTokens')}>· {row.maxTokens.trim()}</span>
                  ) : null}
                </div>
                <div className="flex shrink-0 items-center gap-0.5 opacity-60 transition-opacity group-hover:opacity-100">
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    className="size-8 text-muted-foreground hover:text-foreground"
                    onClick={() => setModelDialog({
                      rowKey: row.key,
                      draft: {
                        id: row.id,
                        name: row.name,
                        reasoning: row.reasoning,
                        contextWindow: row.contextWindow,
                        maxTokens: row.maxTokens,
                        thinkingEfforts: row.thinkingEfforts,
                        thinkingDefault: row.thinkingDefault,
                        imageInput: row.imageInput,
                        supportsTools: row.supportsTools,
                        costInput: row.costInput,
                        costOutput: row.costOutput,
                        costCacheRead: row.costCacheRead,
                        costCacheWrite: row.costCacheWrite,
                        baseUrl: row.baseUrl,
                        api: row.api,
                        omitMaxOutputTokens: row.omitMaxOutputTokens,
                        contextPromotionTarget: row.contextPromotionTarget,
                        compactionModel: row.compactionModel,
                      },
                    })}
                    aria-label={t('settings.providers.omp.models.editModel')}
                    title={t('settings.providers.omp.models.editModel')}
                  >
                    <Icon name="edit-2" className="size-4" />
                  </Button>
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    className="size-8 text-muted-foreground hover:text-foreground"
                    onClick={() => setRows((current) => (current.length > 1 ? current.filter((r) => r.key !== row.key) : current))}
                    disabled={rows.length <= 1}
                    aria-label={t('settings.providers.omp.models.remove')}
                    title={rows.length <= 1 ? t('settings.providers.omp.models.removeDisabled') : t('settings.providers.omp.models.remove')}
                  >
                    <Icon name="close-circle" className="size-4" />
                  </Button>
                </div>
              </div>
            ))}
          </div>
        )}
        {fieldErrors.modelId ? <p className="typography-micro mt-1.5 text-[var(--status-error)]" role="alert">{fieldErrors.modelId}</p> : null}
      </SettingsSection>

      <OmpModelDialog
        open={modelDialog.draft !== null}
        draft={modelDialog.draft}
        isExisting={modelDialog.rowKey !== null}
        onCancel={() => setModelDialog({ rowKey: null, draft: null })}
        onSave={(draft) => {
          setRows((current) => {
            const existing = current.find((r) => r.key === modelDialog.rowKey);
            if (existing) {
              return current.map((r) => (r.key === modelDialog.rowKey
                ? {
                    ...r,
                    id: draft.id,
                    name: draft.name,
                    reasoning: draft.reasoning,
                    contextWindow: draft.contextWindow,
                    maxTokens: draft.maxTokens,
                    thinkingEfforts: draft.thinkingEfforts,
                    thinkingDefault: draft.thinkingDefault,
                    thinkingTouched: true,
                    imageInput: draft.imageInput,
                    supportsTools: draft.supportsTools,
                    costInput: draft.costInput,
                    costOutput: draft.costOutput,
                    costCacheRead: draft.costCacheRead,
                    costCacheWrite: draft.costCacheWrite,
                    baseUrl: draft.baseUrl,
                    api: draft.api,
                    omitMaxOutputTokens: draft.omitMaxOutputTokens,
                    contextPromotionTarget: draft.contextPromotionTarget,
                    compactionModel: draft.compactionModel,
                    dialogTouched: true,
                  }
                : r));
            }
            return [...current.filter((r) => r.id.trim().length > 0), { ...emptyModelRow(), ...draft, thinkingTouched: true }];
          });
          setFieldErrors((c) => ({ ...c, modelId: undefined }));
          setModelDialog({ rowKey: null, draft: null });
        }}
      />

      <div className="sticky bottom-0 -mx-6 mt-auto border-t border-border bg-background/95 px-6 py-3 @3xl:-mx-12 @3xl:px-12 backdrop-blur-sm">
        <div className="mx-auto flex w-full max-w-3xl items-center gap-2">
          <Button type="submit" size="sm" disabled={saving}>
            {saving ? t('settings.providers.omp.actions.saving') : t('settings.providers.omp.actions.save')}
          </Button>
          <Button type="button" variant="outline" size="sm" onClick={onRequestClose} disabled={saving}>
            {t('settings.providers.omp.actions.cancel')}
          </Button>
          {fieldErrors.form ? (
            <p className="typography-micro ml-2 min-w-0 truncate text-[var(--status-error)]" role="alert" title={fieldErrors.form}>{fieldErrors.form}</p>
          ) : null}
        </div>
      </div>
    </form>
  );
};
