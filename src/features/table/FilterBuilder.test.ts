import { describe, expect, it } from 'vitest';

import { completeConditions, describeConditions } from './FilterBuilder';

describe('filter builder', () => {
  it('keeps only complete conditions and trims values', () => {
    expect(
      completeConditions([
        { column: 'status', op: 'eq', value: ' paid ' },
        { column: 'total', op: 'gt', value: '  ' },
        { column: 'email', op: 'is_null', value: 'ignored' },
        { column: '', op: 'eq', value: 'x' },
      ]),
    ).toEqual([
      { column: 'status', op: 'eq', value: 'paid' },
      { column: 'email', op: 'is_null', value: '' },
    ]);
  });

  it('describes conditions with their joiner', () => {
    const conditions = [
      { column: 'status', op: 'in' as const, value: 'paid, sent' },
      { column: 'email', op: 'is_not_null' as const, value: '' },
    ];
    expect(describeConditions(conditions, false)).toBe('status in paid, sent AND email is not NULL');
    expect(describeConditions(conditions, true)).toBe('status in paid, sent OR email is not NULL');
  });
});
