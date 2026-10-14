const integer = new Intl.NumberFormat('en-US');

/** `48213` → `48,213`. Accepts exact decimal strings without converting them to floats. */
export function formatCount(value: number | string): string {
  if (typeof value === 'number') return integer.format(value);
  return /^-?\d+$/.test(value) ? value.replace(/\B(?=(\d{3})+(?!\d))/g, ',') : value;
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`;
  const minutes = Math.floor(ms / 60_000);
  return `${minutes} min ${Math.round((ms % 60_000) / 1000)} s`;
}

/** Running timer: `00:12.4`. */
export function formatTimer(ms: number): string {
  const minutes = Math.floor(ms / 60_000);
  const seconds = (ms % 60_000) / 1000;
  return `${String(minutes).padStart(2, '0')}:${seconds.toFixed(1).padStart(4, '0')}`;
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`;
}

const relative = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });
const dateFormat = new Intl.DateTimeFormat('en', { month: 'short', day: 'numeric' });
const timeFormat = new Intl.DateTimeFormat('en', { hour: '2-digit', minute: '2-digit', hour12: false });

/** "2 min ago", "Yesterday", "Oct 10". */
export function formatRelative(epochMs: number | null | undefined, now = Date.now()): string {
  if (!epochMs) return 'Never';
  const diff = Math.round((epochMs - now) / 1000);
  const abs = Math.abs(diff);
  if (abs < 45) return 'Just now';
  if (abs < 3600) return relative.format(Math.round(diff / 60), 'minute');
  if (abs < 86_400) return relative.format(Math.round(diff / 3600), 'hour');
  if (abs < 7 * 86_400) {
    const days = Math.round(diff / 86_400);
    return days === -1 ? 'Yesterday' : relative.format(days, 'day');
  }
  return dateFormat.format(epochMs);
}

/** "today 09:42", "Oct 10 09:42". */
export function formatTimestamp(epochMs: number, now = Date.now()): string {
  const same = new Date(epochMs).toDateString() === new Date(now).toDateString();
  return `${same ? 'today' : dateFormat.format(epochMs)} ${timeFormat.format(epochMs)}`;
}

export function plural(n: number, one: string, many = `${one}s`): string {
  return `${formatCount(n)} ${n === 1 ? one : many}`;
}
