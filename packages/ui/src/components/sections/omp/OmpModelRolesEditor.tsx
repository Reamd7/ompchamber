/**
 * OmpModelRolesEditor — the omp model-roles editing surface shared by the
 * Engine settings page and the capability-gated DefaultsSettings refactor
 * (spec 06 §5.7 GAP-F3, 01 §5.8 P1).
 *
 * Renders only from an authoritative `/api/omp/models` snapshot (via
 * useOmpModelRoles): role assignment through the existing ModelSelector with
 * per-role source badges (global/project), unassign, `modelRoleStorage`, and
 * `defaultThinkingLevel` read from the schema-driven settings payload
 * (targeted `?keys=` read — the Engine page skips these keys in its generic
 * tab rendering so there is exactly one owner per key).
 *
 * GAP-11 (spec 01 §5.8 REVISED R12): when the models snapshot carries a
 * detected legacy OpenChamber `defaultModel`, an import banner offers the
 * explicit one-click import while the `default` role is unconfigured; a
 * configured role only ever gets the neutral side-by-side. No code path here
 * auto-writes or overwrites a configured role.
 */
import React from 'react';
import { ModelSelector } from '@/components/sections/agents/ModelSelector';
import { Button } from '@/components/ui/button';
import { Icon } from '@/components/icon/Icon';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import {
  SettingsFieldRow,
  SETTINGS_CUSTOM_TRIGGER_CLASS,
  SETTINGS_SELECT_ROW_TRIGGER_CLASS,
  SETTINGS_SELECT_SIZE,
  SETTINGS_FIELDS_STACK_CLASS,
} from '@/components/sections/shared/SettingsSection';
import { useI18n, type I18nKey } from '@/lib/i18n';
import { useOmpModelRoles, type OmpRoleSlot } from '@/hooks/useOmpModelRoles';
import { shouldOfferLegacyImport, type OmpLegacyImportState } from '@/lib/omp/legacyImport';
import type { OmpSettingEntry } from '@/lib/api/omp';
import { reportSettingsSaveState } from '@/lib/persistence';
import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';

const OWNED_SETTING_KEYS = ['defaultThinkingLevel', 'modelRoleStorage'] as const;
type OwnedSettingKey = (typeof OWNED_SETTING_KEYS)[number];
const ROLE_SOURCE_LABEL_KEY: Record<string, I18nKey> = {
  global: 'settings.omp.roles.source.global',
  project: 'settings.omp.roles.source.project',
};

const RoleSourceBadge: React.FC<{ source: string }> = ({ source }) => {
  const { t } = useI18n();
  const labelKey = ROLE_SOURCE_LABEL_KEY[source];
  if (!labelKey) return null;
  return (
    <span className="typography-meta shrink-0 rounded-sm border border-border px-1.5 py-0.5 text-muted-foreground" data-testid="omp-role-source">
      {t(labelKey)}
    </span>
  );
};

const LegacyImportBanner: React.FC<{
  state: OmpLegacyImportState;
  importing: boolean;
  error: string | null;
  onImport: () => void;
}> = ({ state, importing, error, onImport }) => {
  const { t } = useI18n();

  if (state.kind === 'offer') {
    return (
      <div
        className="rounded-md border border-border bg-card p-3 flex items-start gap-3"
        data-settings-item="sessions.legacy-import"
      >
        <Icon name="history" className="h-5 w-5 shrink-0 mt-0.5 text-[var(--status-info)]" />
        <div className="flex-1 min-w-0">
          <p className="typography-label text-foreground">{t('settings.omp.legacyImport.offerTitle')}</p>
          <p className="typography-micro text-muted-foreground mt-0.5">
            {t('settings.omp.legacyImport.offerBody', { model: state.legacyModel })}
          </p>
          {error && <p className="typography-micro mt-1 text-[var(--status-error)]">{error}</p>}
          <Button size="sm" variant="outline" className="mt-2" disabled={importing} onClick={onImport}>
            {importing ? t('settings.omp.legacyImport.importing') : t('settings.omp.legacyImport.import')}
          </Button>
        </div>
      </div>
    );
  }

  if (state.kind === 'comparison') {
    return (
      <p className="typography-meta text-muted-foreground">
        {t('settings.omp.legacyImport.comparison', { legacy: state.legacyModel, current: state.currentModel })}
      </p>
    );
  }

  return null;
};

interface OmpModelRolesEditorProps {
  /** Active project directory; the roles surface never renders without one. */
  directory: string | null;
}

export const OmpModelRolesEditor: React.FC<OmpModelRolesEditorProps> = ({ directory }) => {
  const { t } = useI18n();
  const { ompSettings } = useRuntimeAPIs();
  const { snapshot, roles, pending, reload } = useOmpModelRoles(directory);

  const [ownedEntries, setOwnedEntries] = React.useState<Partial<Record<OwnedSettingKey, OmpSettingEntry>>>({});
  const [roleError, setRoleError] = React.useState<string | null>(null);
  const [importing, setImporting] = React.useState(false);
  const [importError, setImportError] = React.useState<string | null>(null);

  const directoryKey = directory ?? '';

  // Targeted schema read for the two keys this editor owns (the Engine page
  // skips them in its generic tab rendering, so there is no double fetch).
  React.useEffect(() => {
    if (!directoryKey) return;
    let cancelled = false;
    void ompSettings.getSettings({ directory: directoryKey, keys: [...OWNED_SETTING_KEYS] }).then((result) => {
      if (cancelled || !result.ok) return;
      setOwnedEntries({
        defaultThinkingLevel: result.data.keys.defaultThinkingLevel,
        modelRoleStorage: result.data.keys.modelRoleStorage,
      });
    });
    return () => {
      cancelled = true;
    };
  }, [ompSettings, directoryKey]);

  const describeRoleFailure = React.useCallback((unavailable: boolean, rejected?: string): string =>
    unavailable
      ? t('settings.omp.surfaceUnavailable')
      : rejected
        ? t('settings.omp.entry.rejected', { reason: rejected })
        : t('settings.omp.roles.saveFailed'), [t]);

  const commitRole = React.useCallback(async (role: string, value: string | null) => {
    if (!directoryKey) return;
    setRoleError(null);
    reportSettingsSaveState('saving');
    const result = await ompSettings.putModelRole({ directory: directoryKey, role, value });
    if (result.ok) {
      reportSettingsSaveState('saved');
      reload();
      return;
    }
    if (result.unavailable) {
      setRoleError(describeRoleFailure(true));
      return;
    }
    setRoleError(describeRoleFailure(false, result.rejected));
  }, [describeRoleFailure, directoryKey, ompSettings, reload]);

  const commitOwnedSetting = React.useCallback(async (key: OwnedSettingKey, value: string) => {
    if (!directoryKey) return;
    setRoleError(null);
    // Optimistic local value so the select reflects the choice immediately;
    // the PUT response below is the authority that reconciles it.
    setOwnedEntries((current) => {
      const entry = current[key];
      return { ...current, [key]: entry ? { ...entry, value } : { type: 'enum', value } };
    });
    reportSettingsSaveState('saving');
    const result = await ompSettings.putSettings({ directory: directoryKey, changes: { [key]: value } });
    if (result.ok) {
      reportSettingsSaveState('saved');
      // The storage target decides where the NEXT role write lands — refresh
      // so the roles hint stays truthful.
      if (key === 'modelRoleStorage') reload();
      return;
    }
    reportSettingsSaveState('error');
    if (!result.unavailable && result.kind === 'rejected') {
      const reason = result.rejected.find((item) => item.key === key)?.reason;
      setRoleError(describeRoleFailure(false, reason));
      return;
    }
    setRoleError(describeRoleFailure(result.unavailable));
  }, [describeRoleFailure, directoryKey, ompSettings, reload]);

  const defaultSlot = roles.find((role) => role.id === 'default') ?? null;
  const legacyState = shouldOfferLegacyImport(snapshot?.legacyDefaults ?? null, defaultSlot);

  const handleLegacyImport = React.useCallback(async () => {
    if (legacyState.kind !== 'offer' || !directoryKey) return;
    setImporting(true);
    setImportError(null);
    // Explicit user-confirmed write (R12): one assignment of the detected
    // legacy value; the snapshot reload collapses the banner because the
    // role is configured afterwards.
    const result = await ompSettings.putModelRole({
      directory: directoryKey,
      role: 'default',
      value: legacyState.legacyModel,
    });
    setImporting(false);
    reload();
    if (result.ok || result.unavailable) {
      setImportError(result.ok ? null : describeRoleFailure(true));
      return;
    }
    setImportError(describeRoleFailure(false, result.rejected));
  }, [describeRoleFailure, directoryKey, legacyState, ompSettings, reload]);

  if (pending) {
    return null;
  }

  if (!snapshot) {
    // Capability on but no authoritative models snapshot (modelRoles.v1 off,
    // fetch failure, malformed payload): the roles surface never renders from
    // a non-authoritative answer — degrade to a quiet note, not the legacy trio.
    return <p className="typography-meta text-muted-foreground">{t('settings.omp.roles.unavailable')}</p>;
  }

  const storageEntry = ownedEntries.modelRoleStorage;
  const thinkingEntry = ownedEntries.defaultThinkingLevel;

  const renderEnumSelect = (key: OwnedSettingKey, entry: OmpSettingEntry, ariaLabel: string): React.ReactNode => {
    const values = entry.values ?? [];
    const selected = typeof entry.value === 'string' ? entry.value : '';
    return (
      <Select
        value={selected || undefined}
        onValueChange={(value) => {
          void commitOwnedSetting(key, value);
        }}
      >
        <SelectTrigger size={SETTINGS_SELECT_SIZE} className={SETTINGS_SELECT_ROW_TRIGGER_CLASS} aria-label={ariaLabel}>
          <SelectValue placeholder={t('settings.omp.entry.selectPlaceholder')}>
            {(selected || (typeof entry.default === 'string' ? entry.default : '')).replace(/^./, (char) => char.toUpperCase())}
          </SelectValue>
        </SelectTrigger>
        <SelectContent>
          {values.map((value) => (
            <SelectItem key={value} value={value}>
              {value.replace(/^./, (char) => char.toUpperCase())}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    );
  };

  return (
    <div className={SETTINGS_FIELDS_STACK_CLASS} data-testid="omp-model-roles-editor">
      <LegacyImportBanner state={legacyState} importing={importing} error={importError} onImport={() => {
        void handleLegacyImport();
      }} />

      {roles.map((slot: OmpRoleSlot) => (
        <SettingsFieldRow
          key={slot.id}
          settingsItem={slot.id === 'default' ? 'sessions.default-model' : undefined}
          label={slot.name}
          controlClassName="flex items-center gap-2"
        >
          <ModelSelector
            providerId={slot.model?.provider ?? ''}
            modelId={slot.model?.id ?? ''}
            onChange={(providerId, modelId) => {
              if (providerId && modelId) void commitRole(slot.id, `${providerId}/${modelId}`);
            }}
            placeholder={t('settings.omp.roles.notSet')}
            className={SETTINGS_CUSTOM_TRIGGER_CLASS}
          />
          {slot.configured && (
            <Button
              variant="ghost"
              size="icon"
              className="h-8 w-8 px-0 text-muted-foreground hover:text-foreground"
              title={t('settings.omp.roles.clearAria', { role: slot.name })}
              aria-label={t('settings.omp.roles.clearAria', { role: slot.name })}
              onClick={() => {
                void commitRole(slot.id, null);
              }}
            >
              <Icon name="close-circle" className="size-4" />
            </Button>
          )}
          {slot.source && <RoleSourceBadge source={slot.source} />}
        </SettingsFieldRow>
      ))}

      {thinkingEntry && thinkingEntry.values && (
        <SettingsFieldRow
          settingsItem="sessions.default-thinking"
          label={t('settings.omp.roles.thinkingLevel')}
          info={thinkingEntry.ui?.description}
        >
          {renderEnumSelect('defaultThinkingLevel', thinkingEntry, t('settings.omp.roles.thinkingLevel'))}
        </SettingsFieldRow>
      )}

      {storageEntry && storageEntry.values && (
        <SettingsFieldRow
          label={t('settings.omp.roles.storage')}
          info={t('settings.omp.roles.storageHint')}
        >
          {renderEnumSelect('modelRoleStorage', storageEntry, t('settings.omp.roles.storage'))}
        </SettingsFieldRow>
      )}

      {roleError && <p className="typography-meta text-[var(--status-error)]">{roleError}</p>}
    </div>
  );
};
