import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { javascript } from '@codemirror/lang-javascript';
import { bracketMatching, indentOnInput } from '@codemirror/language';
import { EditorState } from '@codemirror/state';
import { EditorView, keymap, lineNumbers } from '@codemirror/view';
import { useEffect, useRef } from 'react';

import { editorTheme, sqlHighlight } from './theme';

const inputTheme = EditorView.theme({
  '&': { backgroundColor: 'var(--surface)', fontSize: '12px', border: '1px solid var(--border-control)', borderRadius: 'var(--radius)' },
  '&.cm-focused': { outline: 'var(--ring-w) solid var(--ring-color)', outlineOffset: 'var(--ring-o)', borderColor: 'var(--accent)' },
  '.cm-scroller': { lineHeight: '1.6', fontFamily: 'var(--font-mono)' },
  '.cm-content': { padding: '8px 0' },
  '.cm-gutters': { backgroundColor: 'var(--surface)', border: 'none' },
});

/**
 * Editable code for document text (MongoDB shell syntax and DynamoDB JSON are
 * both JavaScript-highlighted). ⌘↵ calls `onSubmit`. The editor is created
 * once; `value` only seeds it, so typing never resets the cursor.
 */
export function CodeInput({
  value,
  onChange,
  onSubmit,
  label,
  minHeight = 120,
  maxHeight = 480,
  lines = true,
}: {
  value: string;
  onChange: (value: string) => void;
  onSubmit?: () => void;
  label: string;
  minHeight?: number;
  maxHeight?: number;
  lines?: boolean;
}) {
  const host = useRef<HTMLDivElement>(null);
  const handlers = useRef({ onChange, onSubmit });
  handlers.current = { onChange, onSubmit };

  useEffect(() => {
    const view = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: value,
        extensions: [
          ...(lines ? [lineNumbers()] : []),
          history(),
          indentOnInput(),
          bracketMatching(),
          keymap.of([
            { key: 'Mod-Enter', run: () => (handlers.current.onSubmit?.(), true) },
            indentWithTab,
            ...defaultKeymap,
            ...historyKeymap,
          ]),
          javascript(),
          sqlHighlight,
          editorTheme,
          inputTheme,
          EditorView.lineWrapping,
          EditorView.theme({ '.cm-scroller': { minHeight: `${minHeight}px`, maxHeight: `${maxHeight}px` } }),
          EditorView.contentAttributes.of({ 'aria-label': label }),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) handlers.current.onChange(u.state.doc.toString());
          }),
        ],
      }),
    });
    return () => view.destroy();
    // Seeded once per mount; callers remount (key) to load another document.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return <div ref={host} className="selectable" />;
}
