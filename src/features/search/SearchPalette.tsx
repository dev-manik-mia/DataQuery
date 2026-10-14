import { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';

import type { Node } from '@/bindings/Node';
import type { SavedScript } from '@/bindings/SavedScript';
import { Eyebrow, Spinner } from '@/components/ui';
import { api } from '@/lib/ipc';
import { KIND_MARK } from '@/lib/objects';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './SearchPalette.module.css';

type Result = { type: 'object'; node: Node } | { type: 'script'; script: SavedScript };


/** ⌘K: search tables and views in the connected database, and saved scripts. */
export function SearchPalette() {
  const setOpen = useUi((s) => s.setPaletteOpen);
  const session = useWorkspace((s) => s.session);
  const { openTableTab, openQueryTab } = useWorkspace();
  const [query, setQuery] = useState('');
  const [objects, setObjects] = useState<Node[]>([]);
  const [scripts, setScripts] = useState<SavedScript[]>([]);
  const [searching, setSearching] = useState(false);
  const [index, setIndex] = useState(0);
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    input.current?.focus();
    api.listScripts().then(setScripts).catch(() => setScripts([]));
  }, []);

  useEffect(() => {
    const q = query.trim();
    if (!session || !q) {
      setObjects([]);
      return;
    }
    setSearching(true);
    const timer = window.setTimeout(() => {
      api
        .searchObjects({ sessionId: session.session_id }, q)
        .then(setObjects)
        .catch(() => setObjects([]))
        .finally(() => setSearching(false));
    }, 120);
    return () => window.clearTimeout(timer);
  }, [query, session]);

  const results = useMemo<Result[]>(() => {
    const q = query.trim().toLowerCase();
    const matchingScripts = scripts.filter((s) => !q || s.name.toLowerCase().includes(q) || s.sql.toLowerCase().includes(q)).slice(0, 8);
    return [...objects.map((node) => ({ type: 'object' as const, node })), ...matchingScripts.map((script) => ({ type: 'script' as const, script }))];
  }, [objects, scripts, query]);

  useEffect(() => setIndex(0), [results.length]);

  const close = () => setOpen(false);
  const choose = (r: Result | undefined) => {
    if (!r) return;
    if (r.type === 'object') openTableTab(r.node, 'data');
    else openQueryTab({ sql: r.script.sql, title: r.script.name, scriptId: r.script.id });
    close();
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') setIndex((i) => Math.min(results.length - 1, i + 1));
    else if (e.key === 'ArrowUp') setIndex((i) => Math.max(0, i - 1));
    else if (e.key === 'Enter') choose(results[index]);
    else if (e.key === 'Escape') close();
    else return;
    e.preventDefault();
  };

  const firstScript = results.findIndex((r) => r.type === 'script');

  return createPortal(
    <div className="dq-scrim dq-scrim--top" onMouseDown={(e) => e.target === e.currentTarget && close()}>
      <div className={`dq-dialog ${styles.palette}`} role="dialog" aria-modal="true" aria-label="Search">
        <input
          ref={input}
          className={styles.input}
          role="combobox"
          aria-expanded
          aria-controls="dq-search-results"
          aria-activedescendant={results[index] ? `dq-search-${index}` : undefined}
          placeholder={session ? 'Search tables, views and saved scripts' : 'Search saved scripts'}
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={onKeyDown}
        />
        <div className={styles.list} id="dq-search-results" role="listbox">
          {results.length === 0 && (
            <div className={styles.empty}>{searching ? <Spinner /> : query.trim() ? 'No matches.' : 'Type to search.'}</div>
          )}
          {results.map((r, i) => (
            <div key={r.type === 'object' ? `o${i}${r.node.name}` : `s${r.script.id}`}>
              {i === 0 && r.type === 'object' && (
                <div className={styles.section}>
                  <Eyebrow>Objects in {session?.server.database}</Eyebrow>
                </div>
              )}
              {i === firstScript && (
                <div className={styles.section}>
                  <Eyebrow>Saved scripts</Eyebrow>
                </div>
              )}
              <button
                type="button"
                id={`dq-search-${i}`}
                role="option"
                aria-selected={i === index}
                className={styles.item}
                onMouseEnter={() => setIndex(i)}
                onClick={() => choose(r)}
              >
                {r.type === 'object' ? (
                  <>
                    <span className={styles.kind}>{KIND_MARK[r.node.kind]}</span>
                    <span className="mono">{r.node.name}</span>
                    <span className={styles.detail}>{r.node.schema ?? r.node.database}</span>
                  </>
                ) : (
                  <>
                    <span className={styles.kind}>S</span>
                    <span>{r.script.name}</span>
                    <span className={`${styles.detail} mono ellipsis`} style={{ maxWidth: 260 }}>
                      {r.script.sql.replace(/\s+/g, ' ')}
                    </span>
                  </>
                )}
              </button>
            </div>
          ))}
        </div>
      </div>
    </div>,
    document.body,
  );
}
