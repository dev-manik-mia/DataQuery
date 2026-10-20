import { useMemo, useState } from 'react';

import type { Engine } from '@/bindings/Engine';
import type { EngineGroup } from '@/bindings/EngineGroup';
import type { EngineInfo } from '@/bindings/EngineInfo';
import { Button, EngineMark, Input, PageHead, PriorityChip } from '@/components/ui';
import { useConnections } from '@/stores/connections';
import { useUi } from '@/stores/ui';

import { ImportUrlDialog } from './ImportUrlDialog';
import styles from './Connections.module.css';

const GROUPS: { group: EngineGroup; name: string; model: string }[][] = [
  [{ group: 'relational', name: 'Relational', model: 'SQL' }],
  [
    { group: 'analytical', name: 'Analytical', model: 'SQL / OLAP' },
    { group: 'key_value', name: 'Key-value and cache', model: 'Commands' },
  ],
  [
    { group: 'document', name: 'Document', model: 'MQL / JSON' },
    { group: 'wide_column', name: 'Wide-column', model: 'CQL' },
    { group: 'search_graph', name: 'Search and graph', model: 'DSL / Cypher' },
  ],
];

/** Step 1: choose an engine. Engines without an adapter are shown but not selectable. */
export function EnginePicker() {
  const engines = useConnections((s) => s.engines);
  const navigate = useUi((s) => s.navigate);
  const [query, setQuery] = useState('');
  const [selected, setSelected] = useState<Engine>('postgres');
  const [importing, setImporting] = useState(false);

  const matches = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (e: EngineInfo) => !q || e.name.toLowerCase().includes(q) || e.language.toLowerCase().includes(q);
  }, [query]);

  const chosen = engines.find((e) => e.engine === selected && e.available);
  const proceed = (engine: Engine) => navigate({ name: 'connection-form', engine });

  return (
    <div className={styles.picker}>
      <PageHead
        eyebrow="New connection · step 1 of 2"
        title="Choose a database"
        actions={<Input label="Search engines" placeholder={`Search ${engines.length} engines`} small={false} className={styles.pickerSearch} value={query} onChange={(e) => setQuery(e.target.value)} autoFocus />}
      />
      <div className={styles.engineCols}>
        {GROUPS.map((column, i) => (
          <div key={i} className={styles.engineCol}>
            {column.map(({ group, name, model }) => {
              const items = engines.filter((e) => e.group === group && matches(e));
              if (!items.length) return null;
              return (
                <section key={group} className="dq-card" aria-label={name}>
                  <div className={styles.engineGroupHead}>
                    <span>{name}</span>
                    <span className="muted" style={{ fontWeight: 400, fontSize: 12 }}>
                      {model}
                    </span>
                  </div>
                  {items.map((e) => (
                    <button
                      key={e.engine}
                      type="button"
                      className={styles.engine}
                      aria-pressed={selected === e.engine && e.available}
                      aria-disabled={!e.available}
                      title={e.available ? undefined : `${e.name} is planned (${e.priority})`}
                      onClick={() => e.available && setSelected(e.engine)}
                      onDoubleClick={() => e.available && proceed(e.engine)}
                    >
                      <EngineMark engine={e} size={32} />
                      <span className={styles.engineText}>
                        <span className={styles.name}>{e.name}</span>
                        <span className={`${styles.engineSub} ellipsis`}>{e.summary}</span>
                      </span>
                      <PriorityChip priority={e.priority} available={e.available} beta={e.beta} />
                    </button>
                  ))}
                </section>
              );
            })}
          </div>
        ))}
      </div>
      <div className={styles.pickerFoot}>
        <span>Each engine is a separate adapter with its own explorer, query language and type rendering. Engines marked P0–P2 become selectable when their adapter ships.</span>
        <div style={{ display: 'flex', gap: 8, flex: 'none' }}>
          <Button size="sm" onClick={() => navigate({ name: 'home' })}>
            Cancel
          </Button>
          <Button size="sm" onClick={() => setImporting(true)}>
            Import connection URL
          </Button>
          <Button size="sm" variant="primary" disabled={!chosen} onClick={() => chosen && proceed(chosen.engine)}>
            Continue with {chosen?.name ?? '…'}
          </Button>
        </div>
      </div>
      {importing && <ImportUrlDialog onClose={() => setImporting(false)} />}
    </div>
  );
}
