import { RangeSet, StateEffect, StateField, type EditorState } from '@codemirror/state';
import { EditorView, GutterMarker, gutter } from '@codemirror/view';

/** Replaces every bookmark (1-based line numbers). */
export const setBookmarks = StateEffect.define<number[]>();
/** Toggles the bookmark on the line containing a position. */
export const toggleBookmarkAt = StateEffect.define<number>();

class BookmarkMarker extends GutterMarker {
  toDOM() {
    const el = document.createElement('span');
    el.className = 'dq-bookmark';
    el.textContent = '◆';
    el.setAttribute('aria-label', 'Bookmark');
    return el;
  }
}

const marker = new BookmarkMarker();

export const bookmarkField = StateField.define<RangeSet<GutterMarker>>({
  create: () => RangeSet.empty,
  update(set, tr) {
    let next = set.map(tr.changes);
    for (const e of tr.effects) {
      if (e.is(setBookmarks)) {
        const doc = tr.state.doc;
        const lines = [...new Set(e.value)].filter((n) => n >= 1 && n <= doc.lines).sort((a, b) => a - b);
        next = RangeSet.of(lines.map((n) => marker.range(doc.line(n).from)));
      } else if (e.is(toggleBookmarkAt)) {
        const from = tr.state.doc.lineAt(e.value).from;
        let present = false;
        next.between(from, from, () => {
          present = true;
        });
        next = present ? next.update({ filter: (f) => f !== from }) : next.update({ add: [marker.range(from)] });
      }
    }
    return next;
  },
});

/** Bookmarked line numbers, ascending. */
export function bookmarkedLines(state: EditorState): number[] {
  const lines: number[] = [];
  const iter = state.field(bookmarkField).iter();
  while (iter.value) {
    lines.push(state.doc.lineAt(iter.from).number);
    iter.next();
  }
  return lines;
}

export const bookmarkGutter = [
  bookmarkField,
  gutter({
    class: 'dq-bookmark-gutter',
    markers: (v) => v.state.field(bookmarkField),
    initialSpacer: () => marker,
    domEventHandlers: {
      mousedown(view, line) {
        view.dispatch({ effects: toggleBookmarkAt.of(line.from) });
        return true;
      },
    },
  }),
  EditorView.theme({
    '.dq-bookmark-gutter .cm-gutterElement': { width: '14px', cursor: 'pointer', color: 'var(--accent)', fontSize: '9px', textAlign: 'center' },
  }),
];

/** Moves the cursor to the next (or previous) bookmark, wrapping around. */
export function jumpToBookmark(view: EditorView, direction: 1 | -1): boolean {
  const lines = bookmarkedLines(view.state);
  if (!lines.length) return false;
  const current = view.state.doc.lineAt(view.state.selection.main.head).number;
  const target = direction === 1 ? (lines.find((l) => l > current) ?? lines[0]!) : ([...lines].reverse().find((l) => l < current) ?? lines[lines.length - 1]!);
  const pos = view.state.doc.line(target).from;
  view.dispatch({ selection: { anchor: pos }, scrollIntoView: true });
  return true;
}
