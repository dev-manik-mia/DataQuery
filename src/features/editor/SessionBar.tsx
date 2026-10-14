import { useEffect, useState } from 'react';

import type { QueryAnalysis } from '@/bindings/QueryAnalysis';
import type { SessionOption } from '@/bindings/SessionOption';
import { api } from '@/lib/ipc';
import { useUi } from '@/stores/ui';

import styles from './QueryTab.module.css';

/** Query-connection settings the engine offers (CQL consistency, serial consistency, page size). */
export function SessionOptions({ sessionId }: { sessionId: string }) {
  const [options, setOptions] = useState<SessionOption[]>([]);
  useEffect(() => {
    api
      .sessionOptions({ sessionId })
      .then(setOptions)
      .catch(() => setOptions([]));
  }, [sessionId]);
  const change = async (name: string, value: string) => {
    try {
      setOptions(await api.setSessionOption({ sessionId }, name, value));
    } catch (e) {
      useUi.getState().toastError(e);
    }
  };
  return (
    <>
      {options.map((o) => (
        <label key={o.name} className={styles.meta}>
          {o.label}{' '}
          <select className={styles.metaSelect} value={o.value} onChange={(e) => void change(o.name, e.target.value)}>
            {o.choices.map((c) => (
              <option key={c} value={c}>
                {c}
              </option>
            ))}
          </select>
        </label>
      ))}
    </>
  );
}

const MARK = { good: '✓', info: 'ℹ', caution: '⚠' } as const;

/** The read analysis of the statement being edited (screen 14): single partition, several, or a scan. */
export function AnalysisBanner({ analysis }: { analysis: QueryAnalysis | null }) {
  if (!analysis) return null;
  return (
    <div className={styles.analysis} data-level={analysis.level} role="status">
      <span aria-hidden>{MARK[analysis.level]}</span> {analysis.message}
    </div>
  );
}
