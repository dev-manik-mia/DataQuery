export const isMac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

/** Shortcut label for the current platform: `mod+enter` → `⌘↵` or `Ctrl+↵`. */
export function shortcutLabel(shortcut: string): string {
  const parts = shortcut.toLowerCase().split('+');
  const symbols: Record<string, string> = isMac
    ? { mod: '⌘', shift: '⇧', alt: '⌥', enter: '↵' }
    : { mod: 'Ctrl+', shift: 'Shift+', alt: 'Alt+', enter: '↵' };
  return parts.map((p) => symbols[p] ?? p.toUpperCase()).join('');
}

/** True when a keyboard event matches `mod+shift+k` style shortcuts. */
export function matchShortcut(event: KeyboardEvent, shortcut: string): boolean {
  const parts = shortcut.toLowerCase().split('+');
  const key = parts[parts.length - 1];
  const mod = isMac ? event.metaKey : event.ctrlKey;
  return (
    parts.includes('mod') === mod &&
    parts.includes('shift') === event.shiftKey &&
    parts.includes('alt') === event.altKey &&
    (event.key.toLowerCase() === key || (key === 'enter' && event.key === 'Enter'))
  );
}
