/** Horizontal windowing for the grid: which on-screen columns to render. Pure, so it is unit-tested. */

/** Left edge of each column, plus the right edge of the last: column k spans offsets[k]..offsets[k + 1]. */
export function columnOffsets(widths: readonly number[], start: number): number[] {
  const out = [start];
  for (const w of widths) out.push(out[out.length - 1]! + w);
  return out;
}

/** Last column index whose left edge is at or before x (0 when x is left of everything). */
function columnAt(offsets: readonly number[], x: number): number {
  let lo = 0;
  let hi = offsets.length - 2;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (offsets[mid]! <= x) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

/** Inclusive [first, last] of the columns intersecting [from, to), widened by `overscan` columns each side. */
export function columnRange(offsets: readonly number[], from: number, to: number, overscan: number): [number, number] {
  const count = offsets.length - 1;
  if (count <= 0) return [0, -1];
  const first = columnAt(offsets, from);
  const last = Math.max(first, columnAt(offsets, to - 1));
  return [Math.max(0, first - overscan), Math.min(count - 1, last + overscan)];
}

/**
 * Sorted column indexes to render: the leading `pinned` columns (sticky, so always on
 * screen), the range, and any extra index such as the active or editing cell (-1 = none).
 */
export function columnSlots(count: number, pinned: number, first: number, last: number, extra: readonly number[] = []): number[] {
  const out: number[] = [];
  for (let k = 0; k < Math.min(pinned, count); k += 1) out.push(k);
  for (let k = Math.max(first, pinned); k <= Math.min(last, count - 1); k += 1) out.push(k);
  for (const k of extra) if (k >= 0 && k < count && !out.includes(k)) out.push(k);
  return out.sort((a, b) => a - b);
}

/**
 * The scrollLeft that brings content [start, end) into view, given `sticky` pixels at the
 * left covered by sticky columns; the current scrollLeft when it is already in view.
 */
export function revealScrollLeft(start: number, end: number, scrollLeft: number, viewWidth: number, sticky: number): number {
  if (start < scrollLeft + sticky) return Math.max(0, start - sticky);
  if (end > scrollLeft + viewWidth) return Math.max(0, Math.min(start - sticky, end - viewWidth));
  return scrollLeft;
}
