import { describe, expect, it } from 'vitest';

import type { Cell } from '@/bindings/Cell';
import type { ColumnInfo } from '@/bindings/ColumnInfo';

import { NO_EDITS, addInsert, counts, rowKey, setCell, setInsertCell, toChanges, toggleDeletes } from './edits';

const col = (name: string, extra: Partial<ColumnInfo> = {}): ColumnInfo => ({
  name,
  data_type: 'text',
  nullable: true,
  default: null,
  identity: false,
  primary_key: false,
  generated: false,
  editable: true,
  numeric: false,
  comment: null,
  ...extra,
});

const columns = [col('id', { primary_key: true, data_type: 'bigint' }), col('email'), col('note')];
const rows: Cell[][] = [
  [{ kind: 'number', value: '9007199254740993' }, { kind: 'text', value: 'a@x' }, { kind: 'null' }],
  [{ kind: 'number', value: '2' }, { kind: 'text', value: 'b@x' }, { kind: 'text', value: '' }],
];

describe('table edits', () => {
  it('targets rows by primary key with exact values', () => {
    const key = rowKey(rows[0]!, [0]);
    const edits = setCell(NO_EDITS, key, 1, 'new@x', rows[0]![1]);
    expect(toChanges(edits, columns, [0], rows)).toEqual([
      { op: 'update', key: [{ column: 'id', value: '9007199254740993' }], values: [{ column: 'email', value: 'new@x' }] },
    ]);
  });

  it('drops edits that restore the original value', () => {
    const key = rowKey(rows[0]!, [0]);
    let edits = setCell(NO_EDITS, key, 2, 'x', rows[0]![2]);
    edits = setCell(edits, key, 2, null, rows[0]![2]);
    expect(counts(edits).total).toBe(0);
  });

  it('distinguishes NULL from empty string', () => {
    const key = rowKey(rows[1]!, [0]);
    const edits = setCell(NO_EDITS, key, 2, null, rows[1]![2]);
    expect(toChanges(edits, columns, [0], rows)[0]).toMatchObject({ values: [{ column: 'note', value: null }] });
  });

  it('builds inserts and deletes, and a deleted row drops its updates', () => {
    const k0 = rowKey(rows[0]!, [0]);
    let edits = setCell(NO_EDITS, k0, 1, 'z', rows[0]![1]);
    edits = toggleDeletes(edits, [k0]);
    edits = addInsert(edits);
    edits = setInsertCell(edits, edits.inserts[0]!.id, 1, 'c@x');
    expect(counts(edits)).toEqual({ cells: 0, inserts: 1, deletes: 1, total: 2 });
    expect(toChanges(edits, columns, [0], rows)).toEqual([
      { op: 'insert', values: [{ column: 'email', value: 'c@x' }] },
      { op: 'delete', key: [{ column: 'id', value: '9007199254740993' }] },
    ]);
    expect(counts(toggleDeletes(edits, [k0])).deletes).toBe(0);
  });
});
