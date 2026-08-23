/**
 * OmpModelDialog — edit one model of an engine custom provider
 * (Cherry Studio AddModelDrawer / LobeChat model-modal pattern: the list row
 * stays read-only; details with room live here).
 *
 * Field set follows the omp ModelDefinition schema research (docs/omp-parity
 * + pi-catalog types): identity (id/name), capabilities (reasoning, image
 * input, tools), limits (context/max output), thinking (efforts + default),
 * cost (per-1M rates) and an advanced disclosure (per-model baseUrl/api
 * override, omit max output, context promotion, compaction model). `null`
 * clears a key; keys the dialog never touches survive the server merge.
 */
import React from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Icon } from '@/components/icon/Icon';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import {
  SettingsFieldRow,
  SettingsCheckboxRow,
  SETTINGS_FIELDS_STACK_CLASS,
} from '@/components/sections/shared/SettingsSection';
import { SettingsInfoHint } from '@/components/sections/shared/SettingsInfoHint';

/** Official provider/model configuration docs (models.yml reference). */
const PROVIDER_DOCS_URL = 'https://omp.sh/docs/providers';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { useI18n } from '@/lib/i18n';
import { OMP_MODEL_EFFORTS, OMP_PROVIDER_API_VALUES, type OmpModelEffort } from '@/lib/api/omp';
import { cn } from '@/lib/utils';

export interface OmpModelDraft {
  id: string;
  name: string;
  reasoning: boolean;
  contextWindow: string;
  maxTokens: string;
  thinkingEfforts: OmpModelEffort[];
  thinkingDefault: string;
  /** Capabilities. */
  imageInput: boolean;
  /** Tri-state tool support: '' = unset (inherit/native), 'on' = native, 'off' = XML dialect. */
  supportsTools: '' | 'on' | 'off';
  /** Cost per 1M tokens (USD); empty string = keep file value. */
  costInput: string;
  costOutput: string;
  costCacheRead: string;
  costCacheWrite: string;
  /** Advanced. */
  baseUrl: string;
  api: string;
  omitMaxOutputTokens: boolean;
  contextPromotionTarget: string;
  compactionModel: string;
}

interface OmpModelDialogProps {
  open: boolean;
  draft: OmpModelDraft | null;
  isExisting: boolean;
  onCancel: () => void;
  onSave: (draft: OmpModelDraft) => void;
}

export const OmpModelDialog: React.FC<OmpModelDialogProps> = ({ open, draft, isExisting, onCancel, onSave }) => {
  const { t } = useI18n();
  const [edited, setEdited] = React.useState<OmpModelDraft | null>(draft);
  const [error, setError] = React.useState<string | null>(null);
  const [advancedOpen, setAdvancedOpen] = React.useState(false);

  React.useEffect(() => {
    setEdited(draft);
    setError(null);
  }, [draft]);

  if (!edited) return null;

  const set = (patch: Partial<OmpModelDraft>) => setEdited((current) => (current ? { ...current, ...patch } : current));

  const toggleEffort = (effort: OmpModelEffort) => {
    setEdited((current) => {
      if (!current) return current;
      const has = current.thinkingEfforts.includes(effort);
      const efforts = has
        ? current.thinkingEfforts.filter((e) => e !== effort)
        : [...current.thinkingEfforts, effort];
      const defaultStillSelected = efforts.includes(current.thinkingDefault as OmpModelEffort);
      return {
        ...current,
        thinkingEfforts: efforts,
        thinkingDefault: defaultStillSelected ? current.thinkingDefault : '',
      };
    });
  };

  const handleSave = () => {
    if (!edited.id.trim()) {
      setError(t('settings.providers.omp.error.modelId'));
      return;
    }
    onSave(edited);
  };

  const costField = (key: 'costInput' | 'costOutput' | 'costCacheRead' | 'costCacheWrite', label: string) => (
    <SettingsFieldRow key={key} label={label}>
      <Input
        type="number"
        value={edited[key]}
        onChange={(event) => set({ [key]: event.target.value } as Partial<OmpModelDraft>)}
        placeholder="0"
        className="w-28 font-mono text-xs"
        aria-label={label}
        step="0.01"
        min="0"
      />
    </SettingsFieldRow>
  );

  return (
    <Dialog open={open} onOpenChange={(openNext) => { if (!openNext) onCancel(); }}>
      <DialogContent className="max-h-[85vh] max-w-lg overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{isExisting
            ? t('settings.providers.omp.modelDialog.title.edit')
            : t('settings.providers.omp.modelDialog.title.add')}</DialogTitle>
          <DialogDescription>{t('settings.providers.omp.modelDialog.description')}</DialogDescription>
        </DialogHeader>

        <div className={SETTINGS_FIELDS_STACK_CLASS}>
          <SettingsFieldRow label={<span>{t('settings.providers.omp.models.id')} <span aria-hidden="true" className="text-[var(--status-error)]">*</span></span>}>
            <Input
              value={edited.id}
              onChange={(event) => { set({ id: event.target.value }); setError(null); }}
              placeholder={t('settings.providers.omp.models.id.placeholder')}
              className="flex-1 font-mono text-xs"
              aria-label={t('settings.providers.omp.models.id')}
              aria-required="true"
              aria-invalid={error !== null}
              autoFocus
              spellCheck={false}
            />
          </SettingsFieldRow>
          {error ? <p className="typography-micro text-[var(--status-error)]" role="alert">{error}</p> : null}

          <SettingsFieldRow label={t('settings.providers.omp.models.name')}>
            <Input
              value={edited.name}
              onChange={(event) => set({ name: event.target.value })}
              placeholder={t('settings.providers.omp.models.name.placeholder')}
              className="flex-1 text-xs"
              aria-label={t('settings.providers.omp.models.name')}
            />
          </SettingsFieldRow>

          <SettingsCheckboxRow
            label={t('settings.providers.omp.modelDialog.imageInput')}
            checked={edited.imageInput}
            onChange={(checked) => set({ imageInput: checked })}
          />
          <SettingsCheckboxRow
            label={t('settings.providers.omp.modelDialog.reasoningToggle')}
            checked={edited.reasoning}
            onChange={(checked) => set({ reasoning: checked })}
          />

          <SettingsFieldRow label={t('settings.providers.omp.modelDialog.toolsSupport')}>
            <Select
              value={edited.supportsTools === 'off' ? 'off' : edited.supportsTools === 'on' ? 'on' : undefined}
              onValueChange={(value) => set({ supportsTools: value as '' | 'on' | 'off' })}
            >
              <SelectTrigger className="w-44" size="settings" aria-label={t('settings.providers.omp.modelDialog.toolsSupport')}>
                <SelectValue placeholder={t('settings.providers.omp.modelDialog.toolsAuto')} />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="on">{t('settings.providers.omp.modelDialog.toolsNative')}</SelectItem>
                <SelectItem value="off">{t('settings.providers.omp.modelDialog.toolsXml')}</SelectItem>
              </SelectContent>
            </Select>
          </SettingsFieldRow>

          <SettingsFieldRow label={t('settings.providers.omp.models.context')}>
            <Input
              type="number"
              value={edited.contextWindow}
              onChange={(event) => set({ contextWindow: event.target.value })}
              placeholder="200000"
              className="w-32 font-mono text-xs"
              aria-label={t('settings.providers.omp.models.context')}
            />
          </SettingsFieldRow>

          <SettingsFieldRow label={t('settings.providers.omp.models.maxTokens')}>
            <Input
              type="number"
              value={edited.maxTokens}
              onChange={(event) => set({ maxTokens: event.target.value })}
              placeholder="8192"
              className="w-32 font-mono text-xs"
              aria-label={t('settings.providers.omp.models.maxTokens')}
            />
          </SettingsFieldRow>

          <div className="space-y-2">
            <div className="flex items-center gap-1.5 typography-ui-label text-foreground">
              {t('settings.providers.omp.modelDialog.thinking')}
              <SettingsInfoHint docsUrl={PROVIDER_DOCS_URL}>{t('settings.providers.omp.modelDialog.thinking.info')}</SettingsInfoHint>
            </div>
            <div className="flex flex-wrap gap-1.5">
              {OMP_MODEL_EFFORTS.map((effort) => {
                const selected = edited.thinkingEfforts.includes(effort);
                return (
                  <button
                    key={effort}
                    type="button"
                    onClick={() => toggleEffort(effort)}
                    aria-pressed={selected}
                    className={cn(
                      'rounded-full border px-2.5 py-1 typography-meta font-medium transition-colors',
                      selected
                        ? 'border-transparent bg-interactive-selection/40 text-foreground'
                        : 'border-border/60 text-muted-foreground hover:text-foreground',
                    )}
                  >
                    {effort}
                  </button>
                );
              })}
            </div>
            {edited.thinkingEfforts.length > 0 ? (
              <SettingsFieldRow label={t('settings.providers.omp.modelDialog.thinkingDefault')}>
                <Select
                  value={edited.thinkingDefault || undefined}
                  onValueChange={(value) => set({ thinkingDefault: value })}
                >
                  <SelectTrigger className="w-36" size="settings" aria-label={t('settings.providers.omp.modelDialog.thinkingDefault')}>
                    <SelectValue placeholder={t('settings.providers.omp.modelDialog.thinkingDefault.placeholder')} />
                  </SelectTrigger>
                  <SelectContent>
                    {edited.thinkingEfforts.map((effort) => (
                      <SelectItem key={effort} value={effort}>{effort}</SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </SettingsFieldRow>
            ) : (
              <p className="typography-micro text-muted-foreground">{t('settings.providers.omp.modelDialog.thinkingEmpty')}</p>
            )}
          </div>

          <div className="space-y-2">
            <div className="flex items-center gap-1.5 typography-ui-label text-foreground">
              {t('settings.providers.omp.modelDialog.cost')}
              <SettingsInfoHint docsUrl={PROVIDER_DOCS_URL}>{t('settings.providers.omp.modelDialog.cost.info')}</SettingsInfoHint>
            </div>
            <div className="grid grid-cols-2 gap-x-4 gap-y-2">
              {costField('costInput', t('settings.providers.omp.modelDialog.cost.input'))}
              {costField('costOutput', t('settings.providers.omp.modelDialog.cost.output'))}
              {costField('costCacheRead', t('settings.providers.omp.modelDialog.cost.cacheRead'))}
              {costField('costCacheWrite', t('settings.providers.omp.modelDialog.cost.cacheWrite'))}
            </div>
          </div>

          <div>
            <button
              type="button"
              onClick={() => setAdvancedOpen((prev) => !prev)}
              className="flex w-full items-center gap-1.5 py-1 typography-ui-label text-muted-foreground hover:text-foreground"
              aria-expanded={advancedOpen}
            >
              <Icon name={advancedOpen ? 'arrow-down-s' : 'arrow-right-s'} className="size-3.5" />
              {t('settings.providers.omp.modelDialog.advanced')}
            </button>
            {advancedOpen ? (
              <div className={cn(SETTINGS_FIELDS_STACK_CLASS, 'mt-2 border-l border-border/60 pl-3')}>
                <SettingsFieldRow label={t('settings.providers.omp.modelDialog.advanced.baseUrl')} info={t('settings.providers.omp.modelDialog.advanced.baseUrl.info')} infoDocsUrl={PROVIDER_DOCS_URL}>
                  <Input
                    value={edited.baseUrl}
                    onChange={(event) => set({ baseUrl: event.target.value })}
                    placeholder={t('settings.providers.omp.field.baseUrl.placeholder')}
                    className="flex-1 font-mono text-xs"
                    aria-label={t('settings.providers.omp.modelDialog.advanced.baseUrl')}
                    spellCheck={false}
                  />
                </SettingsFieldRow>
                <SettingsFieldRow label={t('settings.providers.omp.field.api')} info={t('settings.providers.omp.field.api.info')}>
                  <Select value={edited.api || undefined} onValueChange={(value) => set({ api: value })}>
                    <SelectTrigger className="w-full max-w-xs" size="settings" aria-label={t('settings.providers.omp.field.api')}>
                      <SelectValue placeholder={t('settings.providers.omp.modelDialog.advanced.apiInherit')} />
                    </SelectTrigger>
                    <SelectContent>
                      {OMP_PROVIDER_API_VALUES.map((value) => (
                        <SelectItem key={value} value={value}>{value}</SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </SettingsFieldRow>
                <SettingsCheckboxRow
                  label={t('settings.providers.omp.modelDialog.advanced.omitMax')}
                  info={t('settings.providers.omp.modelDialog.advanced.omitMax.info')} infoDocsUrl={PROVIDER_DOCS_URL}
                  checked={edited.omitMaxOutputTokens}
                  onChange={(checked) => set({ omitMaxOutputTokens: checked })}
                />
                <SettingsFieldRow label={t('settings.providers.omp.modelDialog.advanced.promotion')} info={t('settings.providers.omp.modelDialog.advanced.promotion.info')} infoDocsUrl={PROVIDER_DOCS_URL}>
                  <Input
                    value={edited.contextPromotionTarget}
                    onChange={(event) => set({ contextPromotionTarget: event.target.value })}
                    placeholder="provider/model"
                    className="flex-1 font-mono text-xs"
                    aria-label={t('settings.providers.omp.modelDialog.advanced.promotion')}
                    spellCheck={false}
                  />
                </SettingsFieldRow>
                <SettingsFieldRow label={t('settings.providers.omp.modelDialog.advanced.compaction')} info={t('settings.providers.omp.modelDialog.advanced.compaction.info')} infoDocsUrl={PROVIDER_DOCS_URL}>
                  <Input
                    value={edited.compactionModel}
                    onChange={(event) => set({ compactionModel: event.target.value })}
                    placeholder="provider/model"
                    className="flex-1 font-mono text-xs"
                    aria-label={t('settings.providers.omp.modelDialog.advanced.compaction')}
                    spellCheck={false}
                  />
                </SettingsFieldRow>
              </div>
            ) : null}
          </div>
        </div>

        <DialogFooter>
          <Button variant="outline" size="sm" onClick={onCancel}>{t('settings.providers.omp.actions.cancel')}</Button>
          <Button size="sm" onClick={handleSave}>{t('settings.providers.omp.modelDialog.save')}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
