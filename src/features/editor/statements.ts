import { RangeSetBuilder, StateEffect, StateField, type EditorState } from '@codemirror/state';
import { Decoration, EditorView, type DecorationSet } from '@codemirror/view';

import type { StatementInfo } from '@/bindings/StatementInfo';
import { statementAt } from '@/lib/sql';

/** Statement ranges from the backend analyzer for the current document. */
export const setStatements = StateEffect.define<StatementInfo[]>();

export const statementsField = StateField.define<StatementInfo[]>({
  create: () => [],
  update(value, tr) {
    for (const e of tr.effects) if (e.is(setStatements)) return e.value;
    if (!tr.docChanged) return value;
    // Keep ranges roughly aligned until the next analysis arrives.
    return value.map((s) => ({ ...s, from: tr.changes.mapPos(s.from), to: tr.changes.mapPos(s.to, 1) }));
  },
});

const currentLine = Decoration.line({ class: 'dq-current-statement' });

function currentStatementDecorations(state: EditorState): DecorationSet {
  const selection = state.selection.main;
  const builder = new RangeSetBuilder<Decoration>();
  if (!selection.empty) return builder.finish();
  const statements = state.field(statementsField);
  if (statements.length < 2) return builder.finish();
  const current = statementAt(statements, selection.head);
  if (!current) return builder.finish();
  const doc = state.doc;
  const from = doc.lineAt(Math.min(current.from, doc.length)).number;
  const to = doc.lineAt(Math.min(current.to, doc.length)).number;
  for (let n = from; n <= to; n += 1) builder.add(doc.line(n).from, doc.line(n).from, currentLine);
  return builder.finish();
}

/** Tints the lines of the statement that Run statement would execute. */
export const currentStatement = StateField.define<DecorationSet>({
  create: currentStatementDecorations,
  update(value, tr) {
    if (tr.docChanged || tr.selection || tr.effects.some((e) => e.is(setStatements))) return currentStatementDecorations(tr.state);
    return value;
  },
  provide: (f) => EditorView.decorations.from(f),
});

/** Server-reported error position, underlined to the end of the word. */
export const setErrorAt = StateEffect.define<number | null>();

const errorMark = Decoration.mark({ class: 'dq-error-mark' });

export const errorField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(value, tr) {
    for (const e of tr.effects) {
      if (!e.is(setErrorAt)) continue;
      if (e.value === null || e.value < 0 || e.value >= tr.state.doc.length) return Decoration.none;
      const line = tr.state.doc.lineAt(e.value);
      const rest = line.text.slice(e.value - line.from);
      const length = Math.max(1, /^[\w$."]+/.exec(rest)?.[0].length ?? 1);
      return Decoration.set([errorMark.range(e.value, Math.min(line.to, e.value + length))]);
    }
    return tr.docChanged ? Decoration.none : value.map(tr.changes);
  },
  provide: (f) => EditorView.decorations.from(f),
});
