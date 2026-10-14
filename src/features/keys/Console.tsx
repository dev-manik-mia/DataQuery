import { useEffect, useRef, useState, type KeyboardEvent } from 'react';

import { toAppError } from '@/lib/errors';
import { api } from '@/lib/ipc';
import { guarded } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import { useKeyBrowser } from './KeyBrowser';
import styles from './Keys.module.css';

interface Entry {
  id: number;
  prompt: string;
  line: string;
  reply: string;
  error: boolean;
}

const MAX_ENTRIES = 200;

/**
 * `redis-cli`-style console on the session's query connection: `SELECT` and
 * `MULTI` persist, dangerous commands (FLUSHALL, FLUSHDB, DEBUG, KEYS, …) ask first.
 */
export function Console({ full = false }: { full?: boolean }) {
  const session = useWorkspace((s) => s.session)!;
  const reloadKeys = useKeyBrowser((s) => s.reload);
  const [database, setDatabase] = useState(() => Number(session.server.database) || 0);
  const [entries, setEntries] = useState<Entry[]>([]);
  const [line, setLine] = useState('');
  const [busy, setBusy] = useState(false);
  const [history, setHistory] = useState<string[]>([]);
  const [cursor, setCursor] = useState<number | null>(null);
  const out = useRef<HTMLDivElement>(null);
  const prompt = `${session.connection.name}:${database}>`;

  useEffect(() => {
    out.current?.scrollTo({ top: out.current.scrollHeight });
  }, [entries]);

  const push = (entry: Omit<Entry, 'id'>) => setEntries((e) => [...e.slice(-MAX_ENTRIES + 1), { ...entry, id: Date.now() + Math.random() }]);

  const run = async () => {
    const command = line.trim();
    if (!command || busy) return;
    setHistory((h) => [...h.filter((x) => x !== command), command].slice(-100));
    setCursor(null);
    setLine('');
    if (/^clear$/i.test(command)) {
      setEntries([]);
      return;
    }
    setBusy(true);
    try {
      const reply = await guarded((c) => api.keysConsole({ sessionId: session.session_id }, command, c));
      if (reply === null) {
        push({ prompt, line: command, reply: '(not run)', error: false });
        return;
      }
      push({ prompt, line: command, reply: reply.text, error: reply.error });
      setDatabase(reply.database);
      // A write may add, change or remove keys shown in the browser.
      if (!reply.error && (await api.analyzeSql(command, 'redis'))[0]?.writes) reloadKeys();
    } catch (e) {
      push({ prompt, line: command, reply: `(error) ${toAppError(e).info.message}`, error: true });
    } finally {
      setBusy(false);
    }
  };

  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      void run();
    } else if (e.key === 'ArrowUp' && history.length) {
      e.preventDefault();
      const i = cursor === null ? history.length - 1 : Math.max(0, cursor - 1);
      setCursor(i);
      setLine(history[i] ?? '');
    } else if (e.key === 'ArrowDown' && cursor !== null) {
      e.preventDefault();
      const i = cursor + 1;
      setCursor(i < history.length ? i : null);
      setLine(i < history.length ? (history[i] ?? '') : '');
    }
  };

  return (
    <section className={`${styles.console} ${full ? styles.full : ''}`} aria-label="Console">
      <div className={styles.consoleHead}>
        <span className="dq-eyebrow">Console</span>
        <span>FLUSHALL, FLUSHDB, DEBUG and KEYS ask for confirmation · ↑ history · “clear”</span>
      </div>
      <div ref={out} className={styles.consoleOut} role="log" aria-live="polite">
        {entries.map((e) => (
          <div key={e.id}>
            <div>
              <span className={styles.prompt}>{e.prompt}</span> <span className={styles.command}>{e.line}</span>
            </div>
            <div className={styles.reply} data-error={e.error}>
              {e.reply}
            </div>
          </div>
        ))}
      </div>
      <label className={styles.consoleIn}>
        <span className={styles.prompt}>{prompt}</span>
        <input
          value={line}
          spellCheck={false}
          autoComplete="off"
          aria-label="Command"
          placeholder={busy ? 'Running…' : 'HGET user:1042 plan'}
          disabled={busy}
          onChange={(e) => setLine(e.target.value)}
          onKeyDown={onKey}
        />
      </label>
    </section>
  );
}
