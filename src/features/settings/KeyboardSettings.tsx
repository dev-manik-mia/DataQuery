import { useMemo, useState } from 'react';
import { useShallow } from 'zustand/react/shallow';

import { Button, Card, PageHead } from '@/components/ui';
import { appCommands, commandKeys, DEFAULT_KEYS } from '@/features/commands/commands';
import { eventToShortcut, shortcutLabel } from '@/lib/platform';
import { useSettings } from '@/stores/settings';

import styles from './Settings.module.css';

/** Every command with its shortcut; click a shortcut and press keys to change it. */
export function KeyboardSettings() {
  const { settings, update } = useSettings(useShallow((s) => ({ settings: s.settings, update: s.update })));
  const [recording, setRecording] = useState<string | null>(null);
  const [filter, setFilter] = useState('');
  const commands = useMemo(() => appCommands().filter((c) => `${c.group} ${c.title}`.toLowerCase().includes(filter.toLowerCase())), [filter]);
  const bound = useMemo(() => {
    const map = new Map<string, string[]>();
    for (const c of appCommands()) {
      const k = commandKeys(c.id);
      if (k) map.set(k, [...(map.get(k) ?? []), c.id]);
    }
    return map;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [settings.keymap]);

  const setKey = (id: string, keys: string | null) => {
    const keymap = { ...settings.keymap };
    if (keys === null || keys === DEFAULT_KEYS[id]) delete keymap[id];
    else keymap[id] = keys;
    void update({ keymap });
  };

  return (
    <>
      <PageHead eyebrow="Settings" title="Keyboard" />
      <Card
        title="Shortcuts"
        flush
        actions={
          <span style={{ display: 'flex', gap: 8 }}>
            <input className="dq-input dq-input--sm" aria-label="Filter commands" placeholder="Filter" value={filter} onChange={(e) => setFilter(e.target.value)} />
            <Button size="xs" disabled={!Object.keys(settings.keymap).length} onClick={() => void update({ keymap: {} })}>
              Reset all
            </Button>
          </span>
        }
        footer="Click a shortcut, then press the new keys. Escape cancels, Backspace removes the shortcut. Ctrl+Tab cycles tabs and cannot be changed."
      >
        <div className={styles.keymap}>
          {commands.map((c) => {
            const keys = commandKeys(c.id);
            const conflict = keys ? (bound.get(keys) ?? []).filter((id) => id !== c.id) : [];
            const custom = c.id in settings.keymap;
            return (
              <div key={c.id} className={styles.keymapRow}>
                <span className="ink-3" style={{ fontSize: 12 }}>
                  {c.group}
                </span>
                <span>{c.title}</span>
                <button
                  type="button"
                  className={`dq-kbd ${styles.keyButton}`}
                  data-recording={recording === c.id}
                  aria-label={`Shortcut for ${c.title}: ${keys ? shortcutLabel(keys) : 'none'}. Click to change.`}
                  onClick={() => setRecording(c.id)}
                  onBlur={() => setRecording((r) => (r === c.id ? null : r))}
                  onKeyDown={(e) => {
                    if (recording !== c.id) return;
                    e.preventDefault();
                    e.stopPropagation();
                    if (e.key === 'Escape') return setRecording(null);
                    if (e.key === 'Backspace' || e.key === 'Delete') {
                      setKey(c.id, '');
                      return setRecording(null);
                    }
                    const shortcut = eventToShortcut(e.nativeEvent);
                    if (!shortcut) return;
                    setKey(c.id, shortcut);
                    setRecording(null);
                  }}
                >
                  {recording === c.id ? 'Press keys…' : keys ? shortcutLabel(keys) : '—'}
                </button>
                <span className={styles.keyStatus}>
                  {conflict.length > 0 && <span style={{ color: 'var(--caution)' }}>Also used by {conflict.length} other</span>}
                  {custom && (
                    <Button size="xs" variant="plain" onClick={() => setKey(c.id, null)}>
                      Reset
                    </Button>
                  )}
                </span>
              </div>
            );
          })}
        </div>
      </Card>
    </>
  );
}
