export const isMac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
export const isWindows = typeof navigator !== 'undefined' && /Win/.test(navigator.platform || navigator.userAgent);

/** Shortcut label for the current platform: `mod+enter` → `⌘↵` or `Ctrl+↵`. */
export function shortcutLabel(shortcut: string): string {
  const parts = shortcut.toLowerCase().split('+');
  const symbols: Record<string, string> = isMac
    ? { mod: '⌘', shift: '⇧', alt: '⌥', enter: '↵' }
    : { mod: 'Ctrl+', shift: 'Shift+', alt: 'Alt+', enter: '↵' };
  return parts.map((p) => symbols[p] ?? p.toUpperCase()).join('');
}

/** The key of an event, independent of Alt/Option producing special characters. */
function eventKey(event: KeyboardEvent): string {
  if (event.code.startsWith('Key')) return event.code.slice(3).toLowerCase();
  if (event.code.startsWith('Digit')) return event.code.slice(5);
  return event.key.toLowerCase();
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
    (eventKey(event) === key || event.key.toLowerCase() === key)
  );
}

/** The `mod+shift+k` style shortcut of a key press, or null for a bare modifier. */
export function eventToShortcut(event: KeyboardEvent): string | null {
  if (['Meta', 'Control', 'Shift', 'Alt'].includes(event.key)) return null;
  const parts = [];
  if (event.shiftKey) parts.push('shift');
  if (event.altKey) parts.push('alt');
  if (isMac ? event.metaKey : event.ctrlKey) parts.push('mod');
  parts.push(eventKey(event));
  return parts.join('+');
}

/** `mod+shift+enter` → CodeMirror's `Shift-Mod-Enter`. */
export function codeMirrorKey(shortcut: string): string {
  return shortcut
    .split('+')
    .map((p) => (p === 'mod' ? 'Mod' : p === 'shift' ? 'Shift' : p === 'alt' ? 'Alt' : p.length === 1 ? p : p[0]!.toUpperCase() + p.slice(1)))
    .join('-');
}
