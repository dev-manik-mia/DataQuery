/**
 * Multi-selection in the explorer tree: ⌘/Ctrl-click toggles, Shift-click and
 * Shift+arrows select a range from the anchor, like Finder and TablePlus.
 * Pure functions over the visible row keys, so the virtualised tree only
 * renders the result.
 */

export interface TreeSelection {
  /** Selected row keys, in visible order. */
  keys: string[];
  /** Where Shift ranges start: the last row clicked without Shift. */
  anchor: string | null;
}

export const EMPTY_SELECTION: TreeSelection = { keys: [], anchor: null };

export interface ClickModifiers {
  /** ⌘ on macOS, Ctrl elsewhere. */
  toggle: boolean;
  range: boolean;
}

/** The keys between `a` and `b` (inclusive) in visible order; just `b` when `a` is not visible. */
function between(order: string[], a: string | null, b: string): string[] {
  const from = a === null ? -1 : order.indexOf(a);
  const to = order.indexOf(b);
  if (to < 0) return [];
  if (from < 0) return [b];
  return order.slice(Math.min(from, to), Math.max(from, to) + 1);
}

/** Keeps visible order and drops duplicates and rows that are no longer visible. */
function ordered(order: string[], keys: Iterable<string>): string[] {
  const set = new Set(keys);
  return order.filter((k) => set.has(k));
}

/** The selection after clicking `key`. */
export function clickSelection(selection: TreeSelection, key: string, order: string[], mods: ClickModifiers): TreeSelection {
  if (mods.range) {
    const range = between(order, selection.anchor, key);
    const keys = mods.toggle ? ordered(order, [...selection.keys, ...range]) : range;
    return { keys, anchor: selection.anchor ?? key };
  }
  if (mods.toggle) {
    const has = selection.keys.includes(key);
    return { keys: has ? selection.keys.filter((k) => k !== key) : ordered(order, [...selection.keys, key]), anchor: key };
  }
  return { keys: [key], anchor: key };
}

/** Shift+arrow: the range from the anchor to the row the cursor moved to. */
export function extendSelection(selection: TreeSelection, cursor: string, order: string[]): TreeSelection {
  const anchor = selection.anchor ?? cursor;
  return { keys: between(order, anchor, cursor), anchor };
}

/** ⌘/Ctrl+A: every visible row like the cursor's (same depth and kind group). */
export function selectSiblings(order: string[], cursor: string, sameGroup: (key: string) => boolean): TreeSelection {
  return { keys: order.filter(sameGroup), anchor: cursor };
}

/** Space: adds or removes the cursor row. */
export function toggleKey(selection: TreeSelection, key: string, order: string[]): TreeSelection {
  return clickSelection(selection, key, order, { toggle: true, range: false });
}

/** The selection restricted to rows still visible (after collapse, filter or refresh). */
export function visibleSelection(selection: TreeSelection, order: string[]): string[] {
  return ordered(order, selection.keys);
}

/** The objects bulk actions apply to: only when the menu opens on a selected row of a multi-selection. */
export function bulkTargets<T>(selection: TreeSelection, clicked: string, order: string[], lookup: (key: string) => T | undefined): T[] | null {
  const keys = visibleSelection(selection, order);
  if (keys.length < 2 || !keys.includes(clicked)) return null;
  return keys.map(lookup).filter((t): t is T => t !== undefined);
}
