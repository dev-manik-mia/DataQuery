import { autocompletion, closeBrackets, closeBracketsKeymap, closeCompletion, completionKeymap } from '@codemirror/autocomplete';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { bracketMatching, indentOnInput } from '@codemirror/language';
import { highlightSelectionMatches, search, searchKeymap } from '@codemirror/search';
import { Compartment, EditorState, Prec } from '@codemirror/state';
import { drawSelection, EditorView, highlightActiveLineGutter, highlightSpecialChars, keymap, lineNumbers, placeholder } from '@codemirror/view';
import { forwardRef, useEffect, useImperativeHandle, useRef } from 'react';

import type { SqlFlavor } from '@/bindings/SqlFlavor';
import { api } from '@/lib/ipc';
import { statementAt } from '@/lib/sql';

import { useEditorStatus } from './editorStatus';
import { sqlLanguage } from './language';
import { currentStatement, errorField, setErrorAt, setStatements, statementsField } from './statements';
import { editorTheme, sqlHighlight } from './theme';

export interface RunTarget {
  sql: string;
  /** Offset of `sql` inside the document. */
  base: number;
}

export interface SqlEditorHandle {
  /** Selection if any, else the statement at the cursor. */
  statementTarget: () => Promise<RunTarget | null>;
  scriptTarget: () => RunTarget;
  focus: () => void;
}

interface SqlEditorProps {
  value: string;
  flavor: SqlFlavor;
  errorAt: number | null;
  active: boolean;
  onChange: (value: string) => void;
  onRunStatement: () => void;
  onRunScript: () => void;
  onSave: () => void;
}

const ANALYZE_DELAY_MS = 150;

export const SqlEditor = forwardRef<SqlEditorHandle, SqlEditorProps>(function SqlEditor(
  { value, flavor, errorAt, active, onChange, onRunStatement, onRunScript, onSave },
  ref,
) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const callbacks = useRef({ onChange, onRunStatement, onRunScript, onSave });
  callbacks.current = { onChange, onRunStatement, onRunScript, onSave };
  const language = useRef(new Compartment());
  const analyzeTimer = useRef<number>(undefined);

  const analyze = async (doc: string) => {
    const statements = await api.analyzeSql(doc, flavor).catch(() => []);
    if (view.current && view.current.state.doc.toString() === doc) view.current.dispatch({ effects: setStatements.of(statements) });
    return statements;
  };

  useEffect(() => {
    const runKeys = Prec.highest(
      keymap.of([
        { key: 'Mod-Enter', run: (v) => (closeCompletion(v), callbacks.current.onRunStatement(), true) },
        { key: 'Shift-Mod-Enter', run: (v) => (closeCompletion(v), callbacks.current.onRunScript(), true) },
        { key: 'Mod-s', run: () => (callbacks.current.onSave(), true), preventDefault: true },
      ]),
    );
    const state = EditorState.create({
      doc: value,
      extensions: [
        runKeys,
        lineNumbers(),
        highlightActiveLineGutter(),
        highlightSpecialChars(),
        history(),
        drawSelection(),
        indentOnInput(),
        bracketMatching(),
        closeBrackets(),
        autocompletion({ icons: false }),
        highlightSelectionMatches(),
        search({ top: true }),
        keymap.of([...closeBracketsKeymap, ...defaultKeymap, ...searchKeymap, ...historyKeymap, ...completionKeymap, indentWithTab]),
        language.current.of(sqlLanguage(flavor)),
        sqlHighlight,
        editorTheme,
        placeholder('Write SQL here. ⌘↵ runs the statement at the cursor.'),
        statementsField,
        currentStatement,
        errorField,
        EditorView.contentAttributes.of({ 'aria-label': 'SQL editor', spellcheck: 'false', autocorrect: 'off', autocapitalize: 'off' }),
        EditorView.updateListener.of((u) => {
          if (u.docChanged) {
            const doc = u.state.doc.toString();
            callbacks.current.onChange(doc);
            window.clearTimeout(analyzeTimer.current);
            analyzeTimer.current = window.setTimeout(() => void analyze(doc), ANALYZE_DELAY_MS);
          }
          if (u.selectionSet || u.docChanged || u.focusChanged) {
            const head = u.state.selection.main.head;
            const line = u.state.doc.lineAt(head);
            useEditorStatus.getState().set(line.number, head - line.from + 1);
          }
        }),
      ],
    });
    view.current = new EditorView({ state, parent: host.current! });
    void analyze(value);
    return () => {
      window.clearTimeout(analyzeTimer.current);
      view.current?.destroy();
      view.current = null;
    };
    // The editor is created once per tab; later prop changes are applied below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // External value changes (e.g. "Open in editor") replace the document.
  useEffect(() => {
    const v = view.current;
    if (v && v.state.doc.toString() !== value) v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: value } });
  }, [value]);

  useEffect(() => {
    view.current?.dispatch({ effects: language.current.reconfigure(sqlLanguage(flavor)) });
  }, [flavor]);

  useEffect(() => {
    view.current?.dispatch({ effects: setErrorAt.of(errorAt) });
  }, [errorAt]);

  useEffect(() => {
    if (active) {
      view.current?.requestMeasure();
      view.current?.focus();
    }
  }, [active]);

  useImperativeHandle(ref, () => ({
    statementTarget: async () => {
      const v = view.current;
      if (!v) return null;
      const sel = v.state.selection.main;
      if (!sel.empty) return { sql: v.state.sliceDoc(sel.from, sel.to), base: sel.from };
      const doc = v.state.doc.toString();
      const statements = await analyze(doc);
      const target = statementAt(statements, sel.head);
      return target ? { sql: doc.slice(target.from, target.to), base: target.from } : null;
    },
    scriptTarget: () => ({ sql: view.current?.state.doc.toString() ?? '', base: 0 }),
    focus: () => view.current?.focus(),
  }));

  return <div ref={host} style={{ height: '100%', overflow: 'hidden' }} />;
});
