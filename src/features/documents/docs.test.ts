import { describe, expect, it } from 'vitest';

import type { DocField } from '@/bindings/DocField';
import type { Document } from '@/bindings/Document';

import { documentsGrid, pipelineText, rangeLabel } from './docs';

const field = (key: string, type_label: string, value: DocField['value']): DocField => ({ key, type_label, value, children: [], more: 0 });
const doc = (fields: DocField[]): Document => ({ key: '', label: '', size_bytes: null, text: '', truncated: false, fields });

describe('documentsGrid', () => {
  it('unions top-level fields in order and keeps numbers exact', () => {
    const { columns, rows } = documentsGrid([
      doc([field('_id', 'Int32', { kind: 'number', value: '1' }), field('big', 'Int64', { kind: 'number', value: '9007199254740993' })]),
      doc([field('_id', 'Int32', { kind: 'number', value: '2' }), field('note', 'String', { kind: 'text', value: 'x' })]),
    ]);
    expect(columns.map((c) => c.name)).toEqual(['_id', 'big', 'note']);
    expect(columns[1]!.numeric).toBe(true);
    expect(rows[0]!.cells[1]).toEqual({ kind: 'number', value: '9007199254740993' });
    expect(rows[1]!.cells[1]).toEqual({ kind: 'null' });
  });
});

describe('pipelineText', () => {
  it('joins enabled stages into an array', () => {
    expect(
      pipelineText([
        { id: 'a', text: '{ $match: { s: 1 } }', enabled: true },
        { id: 'b', text: '{ $limit: 5 }', enabled: false },
        { id: 'c', text: '{ $count: "n" }', enabled: true },
      ]),
    ).toBe('[\n  { $match: { s: 1 } },\n  { $count: "n" }\n]');
    expect(pipelineText([])).toBe('[]');
  });
});

describe('rangeLabel', () => {
  it('marks estimates', () => {
    expect(rangeLabel(0, 20, { count: 3118, exact: true })).toBe('1–20 of 3,118');
    expect(rangeLabel(20, 5, { count: 900000, exact: false })).toBe('21–25 of ~900,000');
    expect(rangeLabel(0, 0, null)).toBe('No documents');
  });
});
