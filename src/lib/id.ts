let counter = 0;

/** Short unique id for UI-only objects (tabs, toasts). */
export function uid(prefix = 'id'): string {
  counter += 1;
  return `${prefix}-${Date.now().toString(36)}-${counter.toString(36)}`;
}
