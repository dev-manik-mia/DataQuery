import { useEffect, useMemo, useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import type { DocIndex } from '@/bindings/DocIndex';
import type { DocumentPage } from '@/bindings/DocumentPage';
import type { ObjectRef } from '@/bindings/ObjectRef';
import { Button, Card, Checkbox, ErrorAlert, Icon, IconButton, Spinner } from '@/components/ui';
import { CodeInput } from '@/features/editor/CodeInput';
import { copyText } from '@/lib/clipboard';
import { toAppError } from '@/lib/errors';
import { formatBytes, formatDuration, plural } from '@/lib/format';
import { uid } from '@/lib/id';
import { api } from '@/lib/ipc';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import { DocumentTree } from './DocumentTree';
import { STAGE_TEMPLATES, pipelineText, type Stage } from './docs';
import styles from './Documents.module.css';

const PREVIEW_LIMIT = 50;

function useSid() {
  const sessionId = useWorkspace((s) => s.session!.session_id);
  return useMemo(() => ({ sessionId }), [sessionId]);
}

/** Pipeline builder: ordered stages that can be switched off, run with a preview of the output. */
export function AggregationPanel({ object }: { object: ObjectRef }) {
  const sid = useSid();
  const openQueryTab = useWorkspace((s) => s.openQueryTab);
  const toast = useUi((s) => s.toast);
  const [stages, setStages] = useState<Stage[]>([{ id: uid('stage'), text: '{ $match: { } }', enabled: true }]);
  const [result, setResult] = useState<DocumentPage | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  const [running, setRunning] = useState(false);

  const update = (id: string, patch: Partial<Stage>) => setStages((all) => all.map((s) => (s.id === id ? { ...s, ...patch } : s)));
  const move = (index: number, by: number) =>
    setStages((all) => {
      const next = [...all];
      const [stage] = next.splice(index, 1);
      next.splice(index + by, 0, stage!);
      return next;
    });

  const run = async () => {
    setRunning(true);
    setError(null);
    try {
      setResult(await api.documentsAggregate(sid, object, pipelineText(stages), PREVIEW_LIMIT));
    } catch (e) {
      setResult(null);
      setError(toAppError(e).info);
    } finally {
      setRunning(false);
    }
  };

  const shellName = /^[A-Za-z_$][\w$]*$/.test(object.name) ? `db.${object.name}` : `db.getCollection(${JSON.stringify(object.name)})`;

  return (
    <div className={styles.body}>
      <div className={styles.split}>
        <section aria-label="Pipeline">
          <h3 className={styles.panelTitle}>Pipeline · {plural(stages.filter((s) => s.enabled).length, 'stage')}</h3>
          <div className={styles.stages}>
            {stages.map((s, i) => (
              <div key={s.id} className={styles.stage} data-disabled={!s.enabled}>
                <div className={styles.stageHead}>
                  <span className="tabular">Stage {i + 1}</span>
                  <span className={styles.spacer} />
                  <Checkbox label="Enabled" checked={s.enabled} onChange={(v) => update(s.id, { enabled: v })} />
                  <IconButton label={`Move stage ${i + 1} up`} bare disabled={i === 0} onClick={() => move(i, -1)}>
                    <Icon name="chevronDown" size={12} style={{ transform: 'rotate(180deg)' }} />
                  </IconButton>
                  <IconButton label={`Move stage ${i + 1} down`} bare disabled={i === stages.length - 1} onClick={() => move(i, 1)}>
                    <Icon name="chevronDown" size={12} />
                  </IconButton>
                  <IconButton label={`Remove stage ${i + 1}`} bare onClick={() => setStages((all) => all.filter((x) => x.id !== s.id))}>
                    <Icon name="trash" size={12} />
                  </IconButton>
                </div>
                <CodeInput value={s.text} onChange={(text) => update(s.id, { text })} onSubmit={() => void run()} label={`Stage ${i + 1}`} minHeight={40} maxHeight={240} lines={false} />
              </div>
            ))}
          </div>
          <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6, marginTop: 10 }}>
            {STAGE_TEMPLATES.map((t) => (
              <Button key={t.op} size="xs" onClick={() => setStages((all) => [...all, { id: uid('stage'), text: t.text, enabled: true }])}>
                + {t.op}
              </Button>
            ))}
          </div>
          <div style={{ display: 'flex', gap: 8, marginTop: 12 }}>
            <Button variant="primary" size="sm" busy={running} shortcut="⌘↵" onClick={() => void run()}>
              Run pipeline
            </Button>
            <Button size="sm" onClick={() => openQueryTab({ sql: `${shellName}.aggregate(${pipelineText(stages)})`, title: `${object.name}.aggregate` })}>
              Open in editor
            </Button>
            <Button size="sm" onClick={() => void copyText(pipelineText(stages)).then(() => toast('Pipeline copied'))}>
              Copy pipeline
            </Button>
          </div>
        </section>
        <section aria-label="Output">
          <h3 className={styles.panelTitle}>
            Output {result && `· ${plural(result.documents.length, 'document')}${result.next ? ' (first ' + PREVIEW_LIMIT + ')' : ''} · ${formatDuration(result.duration_ms)}`}
          </h3>
          {error && <ErrorAlert error={error} title="The pipeline failed" />}
          {running && !result && <Spinner label="Running" />}
          {result && result.documents.length === 0 && <p className="ink-3">The pipeline returned no documents.</p>}
          {result && <DocumentTree documents={result.documents} onCopy={(d) => void copyText(d.text).then(() => toast('Document copied'))} />}
          {!result && !error && !running && <p className="ink-3">Run the pipeline to preview its first {PREVIEW_LIMIT} documents.</p>}
        </section>
      </div>
    </div>
  );
}

/** Indexes with their keys and options (MongoDB indexes; DynamoDB primary key, GSIs and LSIs). */
export function IndexesPanel({ object }: { object: ObjectRef }) {
  const sid = useSid();
  const [indexes, setIndexes] = useState<DocIndex[] | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  useEffect(() => {
    api
      .documentsIndexes(sid, object)
      .then(setIndexes)
      .catch((e) => setError(toAppError(e).info));
  }, [sid, object]);
  if (error) return <div className={styles.body}><ErrorAlert error={error} title="Could not read indexes" /></div>;
  if (!indexes) return <div className={styles.body}><Spinner label="Reading indexes" /></div>;
  return (
    <div className={styles.body}>
      <Card>
        <div className={`${styles.indexRow} ${styles.indexHead}`}>
          <span>Name</span>
          <span>Keys</span>
          <span>Unique</span>
          <span>Options</span>
          <span>Size</span>
        </div>
        {indexes.map((ix) => (
          <div key={ix.name} className={styles.indexRow}>
            <span className={styles.mono} style={{ fontWeight: 600 }}>
              {ix.name}
            </span>
            <span className={styles.mono}>{ix.keys}</span>
            <span>{ix.unique ? 'Yes' : '—'}</span>
            <span className={styles.mono}>{ix.props.map(([k, v]) => `${k}: ${v}`).join(' · ') || '—'}</span>
            <span className="tabular">{ix.size_bytes === null ? '—' : formatBytes(ix.size_bytes)}</span>
          </div>
        ))}
        {indexes.length === 0 && <div className={styles.indexRow}>No indexes.</div>}
      </Card>
    </div>
  );
}

/** The collection's validation rules (`$jsonSchema`, level and action). */
export function ValidationPanel({ object }: { object: ObjectRef }) {
  const sid = useSid();
  const toast = useUi((s) => s.toast);
  const [rules, setRules] = useState<string | null | undefined>(undefined);
  const [error, setError] = useState<DbError | null>(null);
  useEffect(() => {
    api
      .documentsValidation(sid, object)
      .then(setRules)
      .catch((e) => setError(toAppError(e).info));
  }, [sid, object]);
  return (
    <div className={styles.body}>
      {error && <ErrorAlert error={error} title="Could not read validation rules" />}
      {rules === undefined && !error && <Spinner label="Reading validation rules" />}
      {rules === null && <p className="ink-3">This collection has no validation rules.</p>}
      {rules && (
        <>
          <div style={{ display: 'flex', justifyContent: 'flex-end', marginBottom: 8 }}>
            <Button size="sm" onClick={() => void copyText(rules).then(() => toast('Rules copied'))}>
              Copy
            </Button>
          </div>
          <pre className={styles.json}>{rules}</pre>
        </>
      )}
    </div>
  );
}
