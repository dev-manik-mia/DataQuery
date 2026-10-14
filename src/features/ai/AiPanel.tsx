import { useEffect, useMemo, useState } from 'react';

import type { AiAnswer } from '@/bindings/AiAnswer';
import type { AiDisclosure } from '@/bindings/AiDisclosure';
import type { AiRequest } from '@/bindings/AiRequest';
import type { DbError } from '@/bindings/DbError';
import { Button, ErrorAlert, Icon, IconButton, Segmented, Spinner } from '@/components/ui';
import { toAppError } from '@/lib/errors';
import { api } from '@/lib/ipc';
import { useWorkspace, type QueryTab } from '@/stores/workspace';

import styles from './Ai.module.css';

type Task = AiRequest['task'];

/**
 * Assistant (screen 09). Shows what each request sends before it is sent;
 * generated SQL goes into the editor and is never run automatically.
 */
export function AiPanel({ tab, onClose, onInsert }: { tab: QueryTab; onClose: () => void; onInsert: (sql: string) => void }) {
  const session = useWorkspace((s) => s.session)!;
  const sid = { sessionId: session.session_id };
  const lastError = [...tab.run.messages].reverse().find((m) => m.kind === 'error')?.error;
  const [task, setTask] = useState<Task>('sql');
  const [question, setQuestion] = useState('');
  const [disclosure, setDisclosure] = useState<AiDisclosure | null>(null);
  const [answer, setAnswer] = useState<AiAnswer | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  const [busy, setBusy] = useState(false);

  const request = useMemo<AiRequest | null>(() => {
    switch (task) {
      case 'sql':
        return question.trim() ? { task, question } : null;
      case 'explain_query':
        return tab.sql.trim() ? { task, sql: tab.sql } : null;
      case 'explain_error':
        return lastError ? { task, sql: tab.sql, error: lastError.message } : null;
      case 'summarize_schema':
        return { task };
    }
  }, [task, question, tab.sql, lastError]);

  useEffect(() => {
    if (!request) return setDisclosure(null);
    const timer = window.setTimeout(() => {
      api.aiDisclosure(sid, request).then(setDisclosure).catch((e) => setError(toAppError(e).info));
    }, 300);
    return () => window.clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [request]);

  const ask = async () => {
    if (!request) return;
    setBusy(true);
    setError(null);
    setAnswer(null);
    try {
      setAnswer(await api.aiAsk(sid, request));
    } catch (e) {
      setError(toAppError(e).info);
    } finally {
      setBusy(false);
    }
  };

  return (
    <aside className={styles.panel} aria-label="AI assistant">
      <div className={styles.head}>
        <span className={styles.title}>Assistant</span>
        <span className="ink-3" style={{ fontSize: 12 }}>
          {disclosure ? `${disclosure.provider} · ${disclosure.model}` : ''}
        </span>
        <span style={{ flex: 1 }} />
        <IconButton label="Close assistant" bare onClick={onClose}>
          <Icon name="close" size={12} />
        </IconButton>
      </div>
      <div className={styles.body}>
        <Segmented<Task>
          label="Task"
          value={task}
          onChange={setTask}
          options={[
            { value: 'sql', label: 'Write SQL' },
            { value: 'explain_query', label: 'Explain' },
            { value: 'explain_error', label: 'Error', disabled: !lastError },
            { value: 'summarize_schema', label: 'Schema' },
          ]}
        />
        {task === 'sql' && (
          <textarea
            className="dq-input"
            rows={3}
            placeholder="Ask about this schema, e.g. which customers had the most refunds this month?"
            value={question}
            onChange={(e) => setQuestion(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) void ask();
            }}
            aria-label="Question"
          />
        )}
        {task === 'explain_query' && <p className="ink-2">Explains the SQL in this tab.</p>}
        {task === 'explain_error' && <p className="ink-2">{lastError ? `Explains: ${lastError.message}` : 'Run something that fails first.'}</p>}
        {task === 'summarize_schema' && <p className="ink-2">Summarizes the tables and relationships of {session.server.database}.</p>}
        <Button variant="primary" size="sm" busy={busy} disabled={!request} onClick={() => void ask()}>
          Ask
        </Button>
        {disclosure && (
          <div className={styles.disclosure}>
            <div className="dq-eyebrow">Sent with this request</div>
            <ul>
              {disclosure.sent.map((s) => (
                <li key={s}>{s}</li>
              ))}
            </ul>
            <div className="dq-eyebrow">Not sent</div>
            <ul>
              {disclosure.not_sent.map((s) => (
                <li key={s}>{s}</li>
              ))}
            </ul>
            {disclosure.tables.length > 0 && (
              <details>
                <summary className="ink-3">{disclosure.tables.length} tables · {Math.round(disclosure.characters / 1000)}K characters</summary>
                <p className="mono ink-3" style={{ fontSize: 11 }}>
                  {disclosure.tables.join(', ')}
                </p>
              </details>
            )}
          </div>
        )}
        {busy && <Spinner label="Waiting for the model" />}
        {error && <ErrorAlert error={error} />}
        {answer && (
          <div className={styles.answer}>
            <div className={styles.answerText}>{answer.text}</div>
            {answer.sql && (
              <div style={{ display: 'flex', gap: 6 }}>
                <Button size="sm" onClick={() => onInsert(`-- Drafted by the assistant. Review before running.\n${answer.sql}\n`)}>
                  Insert into editor
                </Button>
              </div>
            )}
            <p className="ink-3" style={{ fontSize: 12 }}>Generated SQL is never run automatically. Writes still ask for confirmation when you run them.</p>
          </div>
        )}
      </div>
    </aside>
  );
}
