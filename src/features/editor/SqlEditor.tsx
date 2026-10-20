import { autocompletion, closeBrackets, closeBracketsKeymap, closeCompletion, completionKeymap } from '@codemirror/autocomplete';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { bracketMatching, indentOnInput } from '@codemirror/language';
import { highlightSelectionMatches, search, searchKeymap } from '@codemirror/search';
import { Compartment, EditorState, Prec } from '@codemirror/state';
import { drawSelection, EditorView, highlightActiveLineGutter, highlightSpecialChars, keymap, lineNumbers, placeholder } from '@codemirror/view';
import { Vim, vim } from '@replit/codemirror-vim';
import { forwardRef, useEffect, useImperativeHandle, useRef } from 'react';

import type { SchemaModel } from '@/bindings/SchemaModel';
import type { Snippet } from '@/bindings/Snippet';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import { api } from '@/lib/ipc';
import { codeMirrorKey, shortcutLabel } from '@/lib/platform';
import { statementAt } from '@/lib/sql';

import { bookmarkedLines, bookmarkGutter, jumpToBookmark, setBookmarks, toggleBookmarkAt } from './bookmarks';
import { useEditorStatus } from './editorStatus';
import { sqlLanguage } from './language';
import { currentStatement, errorField, setErrorAt, setStatements, statementsField } from './statements';
import { editorTheme, sqlHighlight } from './theme';

export interface RunTarget {
  sql: string;
  /** Offset of `sql` inside the document. */
  base: number;
}

export interface StatementTarget extends RunTarget {
  /** Values the statement needs for `$n` / `?` placeholders. */
  paramCount: number;
}

export interface SqlEditorHandle {
  /** Selection if any, else the statement at the cursor. */
  statementTarget: () => Promise<StatementTarget | null>;
  scriptTarget: () => RunTarget;
  focus: () => void;
  toggleBookmark: () => void;
  jumpBookmark: (direction: 1 | -1) => void;
  /** Inserts text at the cursor (replacing the selection). */
  insert: (text: string) => void;
}

/** Shortcuts the editor handles itself, in `mod+enter` form. */
export interface EditorKeys {
  runStatement?: string;
  runScript?: string;
  explain?: string;
  save?: string;
  bookmark?: string;
  nextBookmark?: string;
  prevBookmark?: string;
}

interface SqlEditorProps {
  value: string;
  /** Vim key bindings. */
  vimMode: boolean;
  flavor: SqlFlavor;
  errorAt: number | null;
  active: boolean;
  keys: EditorKeys;
  model: SchemaModel | null;
  snippets: Snippet[];
  bookmarks: number[];
  onChange: (value: string) => void;
  onBookmarks: (lines: number[]) => void;
  onRunStatement: () => void;
  onRunScript: () => void;
  onExplain: () => void;
  onSave: () => void;
}

const ANALYZE_DELAY_MS = 150;

/** `:w` saves the editor it was typed in. */
const vimSave = new WeakMap<EditorView, () => void>();
Vim.defineEx('write', 'w', (cm: { cm6?: EditorView }) => {
  if (cm.cm6) vimSave.get(cm.cm6)?.();
});

// Vim's keymap must come before the others so normal mode sees keys first.
const vimExtension = (on: boolean) => (on ? Prec.highest(vim()) : []);

function editorKeymap(keys: EditorKeys, callbacks: React.RefObject<Pick<SqlEditorProps, 'onRunStatement' | 'onRunScript' | 'onExplain' | 'onSave'>>) {
  const bind = (shortcut: string | undefined, run: (v: EditorView) => boolean) => (shortcut ? [{ key: codeMirrorKey(shortcut), run, preventDefault: true }] : []);
  return Prec.highest(
    keymap.of([
      ...bind(keys.runStatement, (v) => (closeCompletion(v), callbacks.current.onRunStatement(), true)),
      ...bind(keys.runScript, (v) => (closeCompletion(v), callbacks.current.onRunScript(), true)),
      ...bind(keys.explain, () => (callbacks.current.onExplain(), true)),
      ...bind(keys.save, () => (callbacks.current.onSave(), true)),
      ...bind(keys.bookmark, (v) => (v.dispatch({ effects: toggleBookmarkAt.of(v.state.selection.main.head) }), true)),
      ...bind(keys.nextBookmark, (v) => jumpToBookmark(v, 1)),
      ...bind(keys.prevBookmark, (v) => jumpToBookmark(v, -1)),
    ]),
  );
}

export const SqlEditor = forwardRef<SqlEditorHandle, SqlEditorProps>(function SqlEditor(
  { value, vimMode, flavor, errorAt, active, keys, model, snippets, bookmarks, onChange, onBookmarks, onRunStatement, onRunScript, onExplain, onSave },
  ref,
) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const callbacks = useRef({ onChange, onBookmarks, onRunStatement, onRunScript, onExplain, onSave });
  callbacks.current = { onChange, onBookmarks, onRunStatement, onRunScript, onExplain, onSave };
  const language = useRef(new Compartment());
  const keyCompartment = useRef(new Compartment());
  const vimCompartment = useRef(new Compartment());
  const analyzeTimer = useRef<number>(undefined);
  /** The text last passed to `onChange`. */
  const emitted = useRef<string | null>(null);

  const analyze = async (doc: string) => {
    const statements = await api.analyzeSql(doc, flavor).catch(() => []);
    if (view.current && view.current.state.doc.toString() === doc) view.current.dispatch({ effects: setStatements.of(statements) });
    return statements;
  };

  useEffect(() => {
    const state = EditorState.create({
      doc: value,
      extensions: [
        vimCompartment.current.of(vimExtension(vimMode)),
        keyCompartment.current.of(editorKeymap(keys, callbacks)),
        bookmarkGutter,
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
        language.current.of(sqlLanguage(flavor, model, snippets)),
        sqlHighlight,
        editorTheme,
        placeholder(`Write SQL here. ${keys.runStatement ? shortcutLabel(keys.runStatement) : 'Run statement'} runs the statement at the cursor.`),
        statementsField,
        currentStatement,
        errorField,
        EditorView.contentAttributes.of({ 'aria-label': 'SQL editor', spellcheck: 'false', autocorrect: 'off', autocapitalize: 'off' }),
        EditorView.updateListener.of((u) => {
          if (u.docChanged || u.transactions.some((tr) => tr.effects.some((e) => e.is(toggleBookmarkAt) || e.is(setBookmarks)))) {
            const lines = bookmarkedLines(u.state);
            const before = bookmarkedLines(u.startState);
            if (lines.join() !== before.join()) callbacks.current.onBookmarks(lines);
          }
          if (u.docChanged) {
            const doc = u.state.doc.toString();
            emitted.current = doc;
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
    vimSave.set(view.current, () => callbacks.current.onSave());
    view.current.dispatch({ effects: setBookmarks.of(bookmarks) });
    void analyze(value);
    return () => {
      window.clearTimeout(analyzeTimer.current);
      view.current?.destroy();
      view.current = null;
    };
    // The editor is created once per tab; later prop changes are applied below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // External value changes (e.g. "Open in editor") replace the document. The
  // echo of our own edit is skipped without copying the document again.
  useEffect(() => {
    const v = view.current;
    if (v && value !== emitted.current && v.state.doc.toString() !== value) v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: value } });
  }, [value]);

  useEffect(() => {
    view.current?.dispatch({ effects: language.current.reconfigure(sqlLanguage(flavor, model, snippets)) });
  }, [flavor, model, snippets]);

  const keySignature = JSON.stringify(keys);
  useEffect(() => {
    view.current?.dispatch({ effects: keyCompartment.current.reconfigure(editorKeymap(keys, callbacks)) });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [keySignature]);

  useEffect(() => {
    view.current?.dispatch({ effects: vimCompartment.current.reconfigure(vimExtension(vimMode)) });
  }, [vimMode]);

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
      const doc = v.state.doc.toString();
      if (!sel.empty) {
        const text = v.state.sliceDoc(sel.from, sel.to);
        const parts = await api.analyzeSql(text, flavor).catch(() => []);
        return { sql: text, base: sel.from, paramCount: parts.length === 1 ? (parts[0]?.param_count ?? 0) : 0 };
      }
      const statements = await analyze(doc);
      const target = statementAt(statements, sel.head);
      return target ? { sql: doc.slice(target.from, target.to), base: target.from, paramCount: target.param_count } : null;
    },
    scriptTarget: () => ({ sql: view.current?.state.doc.toString() ?? '', base: 0 }),
    focus: () => view.current?.focus(),
    toggleBookmark: () => {
      const v = view.current;
      if (v) v.dispatch({ effects: toggleBookmarkAt.of(v.state.selection.main.head) });
    },
    jumpBookmark: (direction) => {
      if (view.current) jumpToBookmark(view.current, direction);
    },
    insert: (text) => {
      const v = view.current;
      if (!v) return;
      v.dispatch(v.state.replaceSelection(text));
      v.focus();
    },
  }));

  return <div ref={host} style={{ height: '100%', overflow: 'hidden' }} />;
});
