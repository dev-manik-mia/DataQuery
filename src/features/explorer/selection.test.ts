import { describe, expect, it } from 'vitest';

import { bulkTargets, clickSelection, EMPTY_SELECTION, extendSelection, selectSiblings, toggleKey, visibleSelection } from './selection';

const order = ['a', 'b', 'c', 'd', 'e'];
const plain = { toggle: false, range: false };
const toggle = { toggle: true, range: false };
const range = { toggle: false, range: true };

describe('explorer multi-selection', () => {
  it('plain click selects one row and sets the anchor', () => {
    expect(clickSelection({ keys: ['a', 'b'], anchor: 'a' }, 'c', order, plain)).toEqual({ keys: ['c'], anchor: 'c' });
  });

  it('⌘/Ctrl-click toggles rows and keeps visible order', () => {
    let s = clickSelection(EMPTY_SELECTION, 'd', order, plain);
    s = clickSelection(s, 'b', order, toggle);
    expect(s).toEqual({ keys: ['b', 'd'], anchor: 'b' });
    s = clickSelection(s, 'd', order, toggle);
    expect(s.keys).toEqual(['b']);
  });

  it('Shift-click selects the range from the anchor, in either direction', () => {
    const s = clickSelection(EMPTY_SELECTION, 'b', order, plain);
    expect(clickSelection(s, 'd', order, range)).toEqual({ keys: ['b', 'c', 'd'], anchor: 'b' });
    const back = clickSelection(clickSelection(s, 'd', order, range), 'a', order, range);
    expect(back).toEqual({ keys: ['a', 'b'], anchor: 'b' });
  });

  it('⌘-Shift-click adds a range to the selection', () => {
    let s = clickSelection(EMPTY_SELECTION, 'a', order, plain);
    s = clickSelection(s, 'd', order, toggle);
    s = clickSelection(s, 'e', order, { toggle: true, range: true });
    expect(s.keys).toEqual(['a', 'd', 'e']);
  });

  it('Shift-click without an anchor selects just the row', () => {
    expect(clickSelection(EMPTY_SELECTION, 'c', order, range)).toEqual({ keys: ['c'], anchor: 'c' });
  });

  it('Shift+arrows extend from the anchor', () => {
    const s = { keys: ['c'], anchor: 'c' };
    expect(extendSelection(s, 'd', order)).toEqual({ keys: ['c', 'd'], anchor: 'c' });
    expect(extendSelection(extendSelection(s, 'd', order), 'b', order).keys).toEqual(['b', 'c']);
  });

  it('select all and space toggle', () => {
    expect(selectSiblings(order, 'b', (k) => k !== 'a').keys).toEqual(['b', 'c', 'd', 'e']);
    expect(toggleKey({ keys: ['a', 'b'], anchor: 'a' }, 'b', order).keys).toEqual(['a']);
  });

  it('hidden rows drop out of the selection', () => {
    expect(visibleSelection({ keys: ['a', 'x', 'c'], anchor: 'a' }, order)).toEqual(['a', 'c']);
  });

  it('bulk targets only when the menu opens on a selected row of several', () => {
    const s = { keys: ['b', 'c'], anchor: 'b' };
    const lookup = (k: string) => (k === 'zzz' ? undefined : k.toUpperCase());
    expect(bulkTargets(s, 'c', order, lookup)).toEqual(['B', 'C']);
    expect(bulkTargets(s, 'e', order, lookup)).toBeNull();
    expect(bulkTargets({ keys: ['b'], anchor: 'b' }, 'b', order, lookup)).toBeNull();
  });
});
