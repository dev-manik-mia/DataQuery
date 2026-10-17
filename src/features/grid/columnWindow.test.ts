import { describe, expect, it } from 'vitest';

import { columnOffsets, columnRange, columnSlots, revealScrollLeft } from './columnWindow';

describe('column window', () => {
  // Row numbers take 52px; ten 100px columns: column k spans 52 + 100k .. 152 + 100k.
  const offsets = columnOffsets(Array.from({ length: 10 }, () => 100), 52);

  it('computes offsets', () => {
    expect(offsets[0]).toBe(52);
    expect(offsets[3]).toBe(352);
    expect(offsets[10]).toBe(1052);
    expect(columnOffsets([], 52)).toEqual([52]);
  });

  it('finds the columns intersecting the viewport', () => {
    expect(columnRange(offsets, 0, 300, 0)).toEqual([0, 2]);
    // Edges: 152 starts column 1; a viewport ending exactly at 352 does not include column 3.
    expect(columnRange(offsets, 152, 352, 0)).toEqual([1, 2]);
    expect(columnRange(offsets, 151, 353, 0)).toEqual([0, 3]);
    expect(columnRange(offsets, 2000, 2400, 0)).toEqual([9, 9]);
  });

  it('applies overscan within bounds', () => {
    expect(columnRange(offsets, 0, 300, 3)).toEqual([0, 5]);
    expect(columnRange(offsets, 552, 752, 3)).toEqual([2, 9]);
    expect(columnRange(columnOffsets([], 52), 0, 300, 3)).toEqual([0, -1]);
  });

  it('always includes pinned and extra columns, sorted and unique', () => {
    expect(columnSlots(10, 0, 3, 5)).toEqual([3, 4, 5]);
    expect(columnSlots(10, 2, 4, 6)).toEqual([0, 1, 4, 5, 6]);
    expect(columnSlots(10, 2, 0, 3)).toEqual([0, 1, 2, 3]);
    expect(columnSlots(10, 0, 3, 5, [8, -1, 4, 8])).toEqual([3, 4, 5, 8]);
    expect(columnSlots(10, 0, 3, 5, [42])).toEqual([3, 4, 5]);
  });

  it('scrolls a column into view past sticky columns', () => {
    // Viewport 400 wide, 52px of sticky row numbers.
    expect(revealScrollLeft(352, 452, 0, 400, 52)).toBe(52);
    expect(revealScrollLeft(152, 252, 300, 400, 52)).toBe(100);
    expect(revealScrollLeft(152, 252, 0, 400, 52)).toBe(0);
    // Wider than the viewport: its start wins.
    expect(revealScrollLeft(552, 1552, 0, 400, 52)).toBe(500);
    expect(revealScrollLeft(52, 152, 0, 400, 52)).toBe(0);
  });
});
