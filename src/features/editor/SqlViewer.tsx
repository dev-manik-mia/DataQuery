import { EditorState } from '@codemirror/state';
import { EditorView } from '@codemirror/view';
import { useEffect, useRef } from 'react';

import type { SqlFlavor } from '@/bindings/SqlFlavor';

import { sqlLanguage } from './language';
import { editorTheme, sqlHighlight } from './theme';

const viewerTheme = EditorView.theme({
  '&': { backgroundColor: 'var(--sunk)', fontSize: '12px' },
  '.cm-scroller': { lineHeight: '1.7' },
  '.cm-content': { padding: '12px 0' },
  '.cm-line': { padding: '0 16px' },
});

/** Read-only, syntax-highlighted SQL (DDL, change previews). Text stays selectable. */
export function SqlViewer({ value, flavor, label }: { value: string; flavor: SqlFlavor; label: string }) {
  const host = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const view = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: value,
        extensions: [
          EditorState.readOnly.of(true),
          EditorView.editable.of(false),
          EditorView.lineWrapping,
          sqlLanguage(flavor),
          sqlHighlight,
          editorTheme,
          viewerTheme,
          EditorView.contentAttributes.of({ 'aria-label': label }),
        ],
      }),
    });
    return () => view.destroy();
  }, [value, flavor, label]);
  return <div ref={host} style={{ height: '100%', overflow: 'auto' }} className="selectable" />;
}
