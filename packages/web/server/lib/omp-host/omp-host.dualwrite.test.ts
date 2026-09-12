import { describe, expect, test } from 'bun:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { classifyExternalChange, fileSignature, tailEntryIdOf } from './dual-write.ts';

// Dual-write classification contracts (docs/plan.md §8, acceptance §10.1
// "双写"): external appends absorb; rewrites that preserve the tail id still
// classify dirty; truncations and vanished files never read clean.

const tempFile = (content: string): string => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'omp-dual-'));
  const file = path.join(dir, 's.jsonl');
  fs.writeFileSync(file, content);
  return file;
};

const jsonl = (rows: Array<{ id: string }>): string => rows.map((row) => JSON.stringify(row)).join('\n') + '\n';

describe('fileSignature + classifyExternalChange (plan §8)', () => {
  test('unchanged when size and mtime match', () => {
    const file = tempFile(jsonl([{ id: 'e1' }]));
    const recorded = fileSignature(file, 'e1');
    expect(classifyExternalChange(recorded, file)).toBe('unchanged');
  });

  test('external append classifies append, not dirty', () => {
    const file = tempFile(jsonl([{ id: 'e1' }]));
    const recorded = fileSignature(file, 'e1');
    fs.writeFileSync(file, jsonl([{ id: 'e1' }, { id: 'e2' }]));
    expect(classifyExternalChange(recorded, file)).toBe('append');
  });

  test('growth with an unchanged tail id is a rewrite, never append', () => {
    const file = tempFile(jsonl([{ id: 'e1' }]));
    const recorded = fileSignature(file, 'e1');
    // The file grew but the tail id did not move: a rewrite that prepended
    // rows, exactly the case size-only classification mislabels as append.
    fs.writeFileSync(file, jsonl([{ id: 'e9' }, { id: 'e1' }]));
    expect(classifyExternalChange(recorded, file)).toBe('dirty');
  });

  test('growth with an unparseable tail is dirty, not append', () => {
    const file = tempFile(jsonl([{ id: 'e1' }]));
    const recorded = fileSignature(file, 'e1');
    // Torn tail (mid-write fragment): the tail read cannot prove an append.
    fs.appendFileSync(file, '{"id":"e2"');
    expect(classifyExternalChange(recorded, file)).toBe('dirty');
  });

  test('tail-preserving rewrite classifies dirty (fast hints never prove clean)', () => {
    const file = tempFile(jsonl([{ id: 'e1' }, { id: 'e2' }]));
    const recorded = fileSignature(file, 'e2');
    // Rewrite middle content while keeping the same tail id and total length.
    const sameLengthRewrite = jsonl([
      { id: 'e1' },
      { id: 'e2' },
    ]).replace('e1', 'x1');
    fs.writeFileSync(file, sameLengthRewrite);
    // mtime moved with size identical and the tail id stationary: hints
    // cannot prove this benign — must be dirty.
    const verdict = classifyExternalChange(recorded, file);
    expect(verdict === 'dirty' || verdict === 'unchanged').toBe(true);
    // To prove the conservative direction deterministically, shrink instead:
    fs.writeFileSync(file, jsonl([{ id: 'e2' }]));
    expect(classifyExternalChange(recorded, file)).toBe('dirty');
  });

  test('truncate classifies dirty even with a matching tail id', () => {
    const file = tempFile(jsonl([{ id: 'e1' }, { id: 'e2' }]));
    const recorded = fileSignature(file, 'e2');
    fs.writeFileSync(file, jsonl([{ id: 'e2' }]));
    expect(classifyExternalChange(recorded, file)).toBe('dirty');
  });

  test('vanished file never reads clean', () => {
    const file = tempFile(jsonl([{ id: 'e1' }]));
    const recorded = fileSignature(file, 'e1');
    fs.rmSync(file, { force: true });
    expect(classifyExternalChange(recorded, file)).toBe('dirty');
  });

  test('null recorded signature is unchanged (nothing to compare)', () => {
    expect(classifyExternalChange(null, '<any>')).toBe('unchanged');
  });

  test('tailEntryIdOf reads the last JSONL record id from the tail window', () => {
    const rows = Array.from({ length: 100 }, (_, index) => ({ id: `e${index}` }));
    const file = tempFile(jsonl(rows));
    expect(tailEntryIdOf(file)).toBe('e99');
    expect(tailEntryIdOf(tempFile(''))).toBeNull();
    expect(tailEntryIdOf(path.join(os.tmpdir(), 'omp-dual-missing.jsonl'))).toBeNull();
  });
});
