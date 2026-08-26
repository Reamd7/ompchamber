/**
 * GAP-11 legacy default-model import state (spec 01 §5.8 REVISED R12, read by
 * the settings roles editor): the OMPChamber `defaultModel` from settings.json
 * is detected read-only (the models snapshot carries `legacyDefaults`), and an
 * import is offered ONLY while the omp `default` role is unconfigured. A
 * configured role never gets an import action — the two values are shown
 * side-by-side and the user may edit the role explicitly instead. No code path
 * here writes anything; the import itself is the editor's explicit
 * `putModelRole({ role: 'default', … })` call.
 */

/** Structural slice of `OmpModelsSnapshot['legacyDefaults']`. */
export interface OmpLegacyDefaultsSlice {
  defaultModel: string;
  defaultProvider?: string;
}

/** Structural slice of the `default` `OmpRoleSlot`. */
export interface OmpDefaultRoleSlice {
  configured: boolean;
  model: { provider: string; id: string } | null;
}

export type OmpLegacyImportState =
  | { kind: 'none' }
  /** Role unconfigured → the one-click import banner may render. */
  | { kind: 'offer'; legacyModel: string }
  /** Role configured → neutral side-by-side only; no import action (never overwrite). */
  | { kind: 'comparison'; legacyModel: string; currentModel: string };

export const shouldOfferLegacyImport = (
  legacyDefaults: OmpLegacyDefaultsSlice | null | undefined,
  defaultRole: OmpDefaultRoleSlice | null | undefined,
): OmpLegacyImportState => {
  const legacyModel = typeof legacyDefaults?.defaultModel === 'string' ? legacyDefaults.defaultModel.trim() : '';
  if (!legacyModel) return { kind: 'none' };
  if (defaultRole?.configured && defaultRole.model) {
    return {
      kind: 'comparison',
      legacyModel,
      currentModel: `${defaultRole.model.provider}/${defaultRole.model.id}`,
    };
  }
  return { kind: 'offer', legacyModel };
};
