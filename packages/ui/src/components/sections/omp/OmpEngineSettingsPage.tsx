/**
 * OmpEngineSettingsPage — the schema-driven engine settings face (spec 06
 * §5.2/§5.6/§5.7 GAP-F1 UI half + 03 §5.5 GAP-C7 approvals area).
 *
 * Capability-gated on `settings.v1` (master D6-R2: server-adjudicated; the
 * nav entry itself is filtered out by SettingsView when the capability is
 * off/unresolved, and this component renders nothing as a direct-navigation
 * fallback). Every degraded state — probe unresolved, feature off, fetch
 * failed, malformed payload — leaves the legacy settings pages untouched.
 *
 * Rendering is schema-driven end to end: the GET payload's `tabs`/`keys` are
 * the product (06 §5.6), so omp schema additions surface here without UI
 * changes. Three surfaces get dedicated ownership instead of the generic
 * tab dump:
 *  - the model-roles editor owns `modelRoles`, `defaultThinkingLevel`, and
 *    `modelRoleStorage` (shared with the DefaultsSettings refactor);
 *  - the approvals area (C7) owns the approval-related keys the schema
 *    exposes (`tools.approvalMode`, `tools.approval`, `bash.patterns`,
 *    `ask.*`) and hides itself entirely when the schema carries none;
 *  - everything else renders as tab sections with schema labels/groups
 *    (schema-sourced labels render verbatim per spec 06 OQ-F6 v1).
 *
 * Writes commit per-key through PUT with rejected keys surfaced inline;
 * credential entries are write-only masked inputs whose responses only
 * confirm `configured` (R9 — values never echo).
 */
import React from 'react';
import { Button } from '@/components/ui/button';
import { Icon } from '@/components/icon/Icon';
import { Input } from '@/components/ui/input';
import { NumberInput } from '@/components/ui/number-input';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import {
  SettingsCheckboxRow,
  SettingsControlGroup,
  SettingsFieldRow,
  SettingsSection,
  SETTINGS_SELECT_ROW_TRIGGER_CLASS,
  SETTINGS_SELECT_SIZE,
  SETTINGS_CONTROL_CLUSTER_CLASS,
  SETTINGS_CLUSTER_CONTROL_CLASS,
  SETTINGS_FIELDS_STACK_CLASS,
} from '@/components/sections/shared/SettingsSection';
import { SettingsPageLayout } from '@/components/sections/shared/SettingsPageLayout';
import { OmpModelRolesEditor } from '@/components/sections/omp/OmpModelRolesEditor';
import { useOmpFeatureEnabled } from '@/hooks/useOmpFeatureEnabled';
import { useRuntimeAPIs } from '@/hooks/useRuntimeAPIs';
import { useEffectiveDirectory } from '@/hooks/useEffectiveDirectory';
import type { OmpSettingEntry, OmpSettingsSnapshot } from '@/lib/api/omp';
import { reportSettingsSaveState } from '@/lib/persistence';
import { useI18n } from '@/lib/i18n';

/** Keys owned by the model-roles editor (never duplicated in tab sections). */
const ROLES_OWNED_KEYS: Record<string, true> = {
  modelRoles: true,
  defaultThinkingLevel: true,
  modelRoleStorage: true,
};

const isApprovalSettingKey = (key: string): boolean =>
  key === 'tools.approvalMode'
  || key === 'tools.approval'
  || key === 'bash.patterns'
  || key.startsWith('ask.');

/** Scalar commit debounce for free-typed values (numbers) — per-key timers. */
const NUMBER_COMMIT_DEBOUNCE_MS = 500;

interface EntryRowProps {
  entryKey: string;
  entry: OmpSettingEntry;
  error?: string;
  onCommit: (key: string, value: unknown) => void;
}

const EntryRow: React.FC<EntryRowProps> = ({ entryKey, entry, error, onCommit }) => {
  const { t } = useI18n();
  const [textDraft, setTextDraft] = React.useState<string | null>(null);
  const label = entry.ui?.label ?? entryKey;
  const info = entry.ui?.description;
  const disabled = entry.editable === false;
  const value = entry.value;
  const credential = entry.credential === true || entry.writeOnly === true || entry.ui?.secret === true;
  const commitTextDraft = () => {
    if (textDraft === null) return;
    const trimmed = textDraft;
    setTextDraft(null);
    if (trimmed !== (typeof value === 'string' ? value : '')) {
      onCommit(entryKey, trimmed);
    }
  };

  if (entry.type === 'boolean') {
    return (
      <SettingsCheckboxRow
        checked={value === true}
        onChange={(checked) => onCommit(entryKey, checked)}
        label={label}
        ariaLabel={label}
        info={info}
        disabled={disabled}
      />
    );
  }

  if (entry.type === 'enum' && entry.values && entry.values.length > 0) {
    const selected = typeof value === 'string' && entry.values.includes(value) ? value : '';
    return (
      <SettingsFieldRow label={label} info={info} description={error}>
        <Select
          value={selected || undefined}
          onValueChange={(next) => onCommit(entryKey, next)}
          disabled={disabled}
        >
          <SelectTrigger size={SETTINGS_SELECT_SIZE} className={SETTINGS_SELECT_ROW_TRIGGER_CLASS} aria-label={label}>
            <SelectValue placeholder={t('settings.omp.entry.selectPlaceholder')}>
              {(selected || (typeof entry.default === 'string' ? entry.default : '')).replace(/^./, (char) => char.toUpperCase())}
            </SelectValue>
          </SelectTrigger>
          <SelectContent>
            {entry.values.map((optionValue) => (
              <SelectItem key={optionValue} value={optionValue}>
                {optionValue.replace(/^./, (char) => char.toUpperCase())}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </SettingsFieldRow>
    );
  }

  if (entry.type === 'number' && typeof value === 'number') {
    return (
      <SettingsFieldRow label={label} info={info} description={error}>
        <NumberInput
          value={value}
          onValueChange={(next) => onCommit(entryKey, next)}
          disabled={disabled}
          className="h-8 rounded-md px-3"
        />
      </SettingsFieldRow>
    );
  }

  if (entry.type === 'number') {
    return (
      <SettingsFieldRow label={label} info={info} description={error}>
        <NumberInput
          fallbackValue={typeof entry.default === 'number' ? entry.default : 0}
          onValueChange={(next) => onCommit(entryKey, next)}
          disabled={disabled}
          className="h-8 rounded-md px-3"
        />
      </SettingsFieldRow>
    );
  }

  if (entry.type === 'string' || credential) {
    const shown = textDraft ?? (typeof value === 'string' ? value : '');
    return (
      <SettingsFieldRow label={label} info={info} description={error} controlClassName={SETTINGS_CONTROL_CLUSTER_CLASS}>
        <div className="flex w-full items-center gap-2">
          <Input
            type={credential ? 'password' : 'text'}
            className={`h-8 rounded-md px-3 ${SETTINGS_CLUSTER_CONTROL_CLASS}`}
            value={credential ? (textDraft ?? '') : shown}
            placeholder={credential && entry.configured ? t('settings.omp.entry.credentialConfigured') : ''}
            disabled={disabled}
            autoComplete="off"
            onChange={(event) => setTextDraft(event.target.value)}
            onBlur={() => {
              // Empty credential drafts never commit — clearing an existing
              // credential needs the explicit button below (TUI semantics:
              // empty string clears, so an accidental blur must not).
              if (!credential || (textDraft ?? '').length > 0) commitTextDraft();
            }}
            aria-label={label}
          />
          {credential && entry.configured === true && (
            <Button
              variant="ghost"
              size="icon"
              className="h-8 w-8 px-0 text-muted-foreground hover:text-foreground"
              title={t('settings.omp.roles.clearAria', { role: label })}
              aria-label={t('settings.omp.entry.clearCredentialAria', { label })}
              disabled={disabled}
              onClick={() => onCommit(entryKey, '')}
            >
              <Icon name="close-circle" className="size-4" />
            </Button>
          )}
          {credential && entry.configured !== true && (
            <span className="typography-meta shrink-0 text-muted-foreground">
              {t('settings.omp.entry.credentialNotSet')}
            </span>
          )}
        </div>
      </SettingsFieldRow>
    );
  }

  // Structured values (arrays/records such as `tools.approval` or
  // `bash.patterns`) render read-only in v1 — editing them needs the
  // dedicated matrix editors deferred with spec 06 §5.6 (v2 items).
  const structured = value === undefined || value === null || value === '' ? '' : JSON.stringify(value);
  return (
    <SettingsFieldRow label={label} info={info}>
      <span className="typography-meta max-w-[24rem] truncate text-muted-foreground" title={structured}>
        {structured ? structured : '—'}
      </span>
      <span className="typography-meta shrink-0 text-muted-foreground/70">{t('settings.omp.entry.readOnly')}</span>
    </SettingsFieldRow>
  );
};

export const OmpEngineSettingsPage: React.FC = () => {
  const { t } = useI18n();
  const settingsEnabled = useOmpFeatureEnabled('settings.v1');
  const { ompSettings } = useRuntimeAPIs();
  const directory = useEffectiveDirectory() ?? null;

  const [snapshot, setSnapshot] = React.useState<OmpSettingsSnapshot | null>(null);
  const [loadState, setLoadState] = React.useState<'pending' | 'ready' | 'failed' | 'unavailable'>('pending');
  const [keyErrors, setKeyErrors] = React.useState<Record<string, string>>({});
  const [pageError, setPageError] = React.useState<string | null>(null);
  const commitTimers = React.useRef(new Map<string, ReturnType<typeof setTimeout>>());

  const directoryKey = directory ?? '';

  React.useEffect(() => {
    if (!settingsEnabled || !directoryKey) return;
    let cancelled = false;
    setLoadState('pending');
    void ompSettings.getSettings({ directory: directoryKey }).then((result) => {
      if (cancelled) return;
      if (result.ok) {
        setSnapshot(result.data);
        setLoadState('ready');
        return;
      }
      setSnapshot(null);
      setLoadState(result.unavailable ? 'unavailable' : 'failed');
    });
    return () => {
      cancelled = true;
    };
  }, [ompSettings, directoryKey, settingsEnabled]);

  React.useEffect(() => {
    const timers = commitTimers.current;
    return () => {
      for (const timer of timers.values()) clearTimeout(timer);
      timers.clear();
    };
  }, []);

  if (!settingsEnabled) {
    // Nav hides this page when the capability is off; this only catches
    // direct navigation (persisted slug / search) against an off engine.
    return null;
  }

  const commitEntry = (key: string, value: unknown): void => {
    if (!directoryKey) return;
    setKeyErrors((current) => {
      if (!(key in current)) return current;
      const next = { ...current };
      delete next[key];
      return next;
    });
    reportSettingsSaveState('saving');
    void ompSettings.putSettings({ directory: directoryKey, changes: { [key]: value } }).then((result) => {
      if (result.ok) {
        // The PUT response is the post-write authority: apply `applied`
        // locally instead of refetching (credential entries only ever
        // confirm `configured` — R9).
        setSnapshot((current) => {
          if (!current) return current;
          const keys = { ...current.keys };
          for (const [appliedKey, appliedValue] of Object.entries(result.applied)) {
            const entry = keys[appliedKey];
            if (!entry) continue;
            if (entry.credential === true || entry.writeOnly === true) {
              keys[appliedKey] = {
                ...entry,
                configured: typeof appliedValue === 'object' && appliedValue !== null
                  ? (appliedValue as { configured?: unknown }).configured === true
                  : false,
              };
            } else {
              keys[appliedKey] = { ...entry, value: appliedValue };
            }
          }
          return { ...current, keys, revision: result.revision };
        });
        reportSettingsSaveState('saved');
        return;
      }
      reportSettingsSaveState('error');
      if (result.unavailable) {
        setPageError(t('settings.omp.surfaceUnavailable'));
        return;
      }
      if (result.kind === 'rejected') {
        const nextErrors: Record<string, string> = {};
        for (const rejection of result.rejected) {
          nextErrors[rejection.key] = rejection.reason ?? 'rejected';
        }
        setKeyErrors((current) => ({ ...current, ...nextErrors }));
        return;
      }
      if (result.kind === 'quarantined') {
        setPageError(t('settings.omp.page.quarantined'));
      }
    });
  };

  const commitEntryDebounced = (key: string, value: unknown): void => {
    const timers = commitTimers.current;
    const existing = timers.get(key);
    if (existing) clearTimeout(existing);
    timers.set(key, setTimeout(() => {
      timers.delete(key);
      commitEntry(key, value);
    }, NUMBER_COMMIT_DEBOUNCE_MS));
  };

  const handleCommit = (key: string, value: unknown): void => {
    const entry = snapshot?.keys[key];
    if (entry?.type === 'number') {
      commitEntryDebounced(key, value);
      return;
    }
    commitEntry(key, value);
  };

  if (loadState === 'pending') {
    return null;
  }

  if (loadState !== 'ready' || !snapshot) {
    return (
      <SettingsPageLayout title={t('settings.page.engine.title')} showSaveStatus={false}>
        <p className="typography-meta text-muted-foreground">
          {t(loadState === 'unavailable' ? 'settings.omp.surfaceUnavailable' : 'settings.omp.page.loadFailed')}
        </p>
      </SettingsPageLayout>
    );
  }

  const isRenderable = (entry: OmpSettingEntry): boolean =>
    !entry.excluded && entry.hidden !== true && typeof entry.ui?.tab === 'string';

  const entries = Object.entries(snapshot.keys).filter(([key, entry]) =>
    ROLES_OWNED_KEYS[key] !== true && !isApprovalSettingKey(key) && isRenderable(entry));

  const approvalEntries = Object.entries(snapshot.keys).filter(([key, entry]) =>
    isApprovalSettingKey(key) && isRenderable(entry));

  const renderEntry = (key: string, entry: OmpSettingEntry): React.ReactNode => (
    <EntryRow
      key={key}
      entryKey={key}
      entry={entry}
      error={keyErrors[key]}
      onCommit={handleCommit}
    />
  );

  return (
    <SettingsPageLayout
      title={t('settings.page.engine.title')}
      description={t('settings.page.engine.description')}
      showSaveStatus
    >
      {pageError && <p className="typography-meta text-[var(--status-error)]">{pageError}</p>}

      <SettingsSection title={t('settings.omp.roles.title')} divider={false} settingsItem="engine.model-roles">
        <OmpModelRolesEditor directory={directory} />
      </SettingsSection>

      {approvalEntries.length > 0 && (
        <SettingsSection title={t('settings.omp.approvals.title')} settingsItem="engine.approvals">
          <div className={SETTINGS_FIELDS_STACK_CLASS}>
            {approvalEntries.map(([key, entry]) => renderEntry(key, entry))}
          </div>
        </SettingsSection>
      )}

      {snapshot.tabs.map((tab) => {
        const tabEntries = entries.filter(([, entry]) => entry.ui?.tab === tab.id);
        if (tabEntries.length === 0) return null;
        const groupNames = [
          ...tab.groups,
          ...Array.from(new Set(tabEntries.map(([, entry]) => entry.ui?.group).filter((group): group is string => !!group)))
            .filter((group) => !tab.groups.includes(group)),
        ];
        return (
          <SettingsSection key={tab.id} title={tab.label}>
            {groupNames.map((group) => {
              const groupEntries = tabEntries.filter(([, entry]) => (entry.ui?.group ?? '') === group);
              if (groupEntries.length === 0) return null;
              return (
                <SettingsControlGroup key={group} title={group} contentClassName={SETTINGS_FIELDS_STACK_CLASS}>
                  {groupEntries.map(([key, entry]) => renderEntry(key, entry))}
                </SettingsControlGroup>
              );
            })}
          </SettingsSection>
        );
      })}
    </SettingsPageLayout>
  );
};

