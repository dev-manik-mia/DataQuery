import { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';

import styles from '@/features/search/SearchPalette.module.css';
import { t } from '@/i18n';
import { shortcutLabel } from '@/lib/platform';
import { useUi } from '@/stores/ui';

import { appCommands, commandKeys, type AppCommand } from './commands';

/** Subsequence match score: lower is better; `null` when `query` is not a subsequence. */
export function fuzzyScore(text: string, query: string): number | null {
  const hay = text.toLowerCase();
  let pos = -1;
  let score = 0;
  for (const ch of query.toLowerCase().replace(/\s+/g, '')) {
    const next = hay.indexOf(ch, pos + 1);
    if (next < 0) return null;
    score += next - pos - 1;
    pos = next;
  }
  return score;
}

/** ⇧⌘P: every available command, filtered as you type, with its shortcut. */
export function CommandPalette() {
  const setOpen = useUi((s) => s.setCommandsOpen);
  const [query, setQuery] = useState('');
  const [index, setIndex] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const commands = useMemo(() => appCommands().filter((c) => c.available), []);

  const results = useMemo(() => {
    const q = query.trim();
    if (!q) return commands;
    return commands
      .map((c) => ({ c, score: fuzzyScore(`${c.group} ${c.title}`, q) }))
      .filter((r): r is { c: AppCommand; score: number } => r.score !== null)
      .sort((a, b) => a.score - b.score)
      .map((r) => r.c);
  }, [commands, query]);

  useEffect(() => input.current?.focus(), []);
  useEffect(() => setIndex(0), [query]);

  const close = () => setOpen(false);
  const run = (c: AppCommand | undefined) => {
    if (!c) return;
    close();
    c.run();
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') setIndex((i) => Math.min(results.length - 1, i + 1));
    else if (e.key === 'ArrowUp') setIndex((i) => Math.max(0, i - 1));
    else if (e.key === 'Enter') run(results[index]);
    else if (e.key === 'Escape') close();
    else return;
    e.preventDefault();
  };

  return createPortal(
    <div className="dq-scrim dq-scrim--top" onMouseDown={(e) => e.target === e.currentTarget && close()}>
      <div className={`dq-dialog ${styles.palette}`} role="dialog" aria-modal="true" aria-label={t('nav.commandPalette')}>
        <input
          ref={input}
          className={styles.input}
          role="combobox"
          aria-expanded
          aria-controls="dq-command-results"
          aria-activedescendant={results[index] ? `dq-command-${index}` : undefined}
          placeholder={t('palette.placeholder')}
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={onKeyDown}
        />
        <div className={styles.list} id="dq-command-results" role="listbox">
          {results.length === 0 && <div className={styles.empty}>{t('palette.empty')}</div>}
          {results.map((c, i) => {
            const keys = commandKeys(c.id);
            return (
              <button
                key={c.id}
                type="button"
                id={`dq-command-${i}`}
                role="option"
                aria-selected={i === index}
                className={styles.item}
                onMouseEnter={() => setIndex(i)}
                onClick={() => run(c)}
              >
                <span className="ink-3" style={{ width: 92, flex: 'none', fontSize: 12 }}>
                  {c.group}
                </span>
                <span>{c.title}</span>
                {keys && <span className={`${styles.detail} dq-kbd`}>{shortcutLabel(keys)}</span>}
              </button>
            );
          })}
        </div>
      </div>
    </div>,
    document.body,
  );
}
