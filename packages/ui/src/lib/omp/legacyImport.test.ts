import { describe, expect, test } from 'bun:test';
import { shouldOfferLegacyImport } from './legacyImport';

describe('shouldOfferLegacyImport (GAP-11, spec 01 §5.8 R12)', () => {
  test('no detected legacy default → nothing renders', () => {
    expect(shouldOfferLegacyImport(null, null)).toEqual({ kind: 'none' });
    expect(shouldOfferLegacyImport({ defaultModel: '' }, null)).toEqual({ kind: 'none' });
    expect(shouldOfferLegacyImport({ defaultModel: '   ' }, null)).toEqual({ kind: 'none' });
  });

  test('legacy default + unconfigured role → offer the explicit import', () => {
    expect(
      shouldOfferLegacyImport({ defaultModel: 'anthropic/claude-sonnet-4' }, { configured: false, model: null }),
    ).toEqual({ kind: 'offer', legacyModel: 'anthropic/claude-sonnet-4' });
    // Absent role slot (snapshot with no default entry) is still an offer.
    expect(shouldOfferLegacyImport({ defaultModel: 'prov/main' }, null))
      .toEqual({ kind: 'offer', legacyModel: 'prov/main' });
  });

  test('configured role → comparison only, never an import action (never overwrite)', () => {
    expect(
      shouldOfferLegacyImport(
        { defaultModel: 'anthropic/claude-sonnet-4' },
        { configured: true, model: { provider: 'openai', id: 'gpt-5' } },
      ),
    ).toEqual({
      kind: 'comparison',
      legacyModel: 'anthropic/claude-sonnet-4',
      currentModel: 'openai/gpt-5',
    });
  });
});
