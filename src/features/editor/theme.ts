import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { EditorView } from '@codemirror/view';
import { tags as t } from '@lezer/highlight';

/** Editor chrome drawn entirely from the design tokens, so it follows the theme. */
export const editorTheme = EditorView.theme({
  '&': {
    height: '100%',
    backgroundColor: 'var(--surface)',
    color: 'var(--ink)',
    fontSize: 'var(--editor-font-size, 13px)',
  },
  '.cm-scroller': { fontFamily: 'var(--font-mono)', lineHeight: '24px' },
  '.cm-content': { padding: '8px 0', caretColor: 'var(--ink)' },
  '.cm-line': { padding: '0 14px' },
  '.cm-gutters': {
    backgroundColor: 'var(--bg)',
    color: 'var(--ink-3)',
    border: 'none',
    borderRight: '1px solid var(--line-2)',
  },
  '.cm-lineNumbers .cm-gutterElement': { minWidth: '34px', padding: '0 10px 0 6px' },
  '.cm-activeLineGutter': { backgroundColor: 'transparent', color: 'var(--ink)' },
  '.cm-cursor': { borderLeftColor: 'var(--ink)', borderLeftWidth: '2px' },
  '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection': { backgroundColor: 'var(--accent-tint) !important' },
  '.cm-matchingBracket': { backgroundColor: 'var(--accent-tint)', outline: '1px solid var(--accent)' },
  '.rb-current-statement': { backgroundColor: 'var(--sunk)' },
  '.rb-error-mark': { textDecoration: 'underline wavy var(--critical)', textUnderlineOffset: '4px' },
  '.cm-searchMatch': { backgroundColor: 'var(--caution-tint)', outline: '1px solid var(--caution)' },
  '.cm-searchMatch-selected': { backgroundColor: 'var(--accent-tint)' },
  '.cm-panels': { backgroundColor: 'var(--surface)', color: 'var(--ink)', borderColor: 'var(--line)' },
  '.cm-panels.cm-panels-top': { borderBottom: '1px solid var(--line)' },
  '.cm-panel.cm-search': { fontFamily: 'var(--font-sans)', fontSize: '12px', padding: '6px 10px' },
  '.cm-panel.cm-search input, .cm-panel.cm-search button': {
    fontSize: '12px',
    borderRadius: 'var(--radius)',
    border: '1px solid var(--line)',
    background: 'var(--bg)',
    color: 'var(--ink)',
  },
  '.cm-panel.cm-search label': { fontSize: '12px' },
  '.cm-tooltip': { backgroundColor: 'var(--surface)', border: '1px solid var(--line)', borderRadius: 'var(--radius)' },
  '.cm-tooltip-autocomplete ul li[aria-selected]': { backgroundColor: 'var(--nav-active-bg)', color: 'var(--nav-active-ink)' },
});

export const sqlHighlight = syntaxHighlighting(
  HighlightStyle.define([
    { tag: [t.keyword, t.operatorKeyword, t.modifier], color: 'var(--syn-kw)', fontWeight: '600' },
    { tag: [t.typeName, t.standard(t.name)], color: 'var(--syn-type)' },
    { tag: [t.string, t.special(t.string)], color: 'var(--positive)' },
    { tag: [t.number, t.bool, t.null], color: 'var(--caution)' },
    { tag: [t.lineComment, t.blockComment, t.comment], color: 'var(--syn-comment)', fontStyle: 'italic' },
    { tag: [t.special(t.name), t.quote], color: 'var(--accent-text)' },
  ]),
);
