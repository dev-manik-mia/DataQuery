import { useMemo, useState } from 'react';

import type { ObjectRef } from '@/bindings/ObjectRef';
import type { SchemaChange } from '@/bindings/SchemaChange';
import { Alert, Checkbox, TextField } from '@/components/ui';
import { objectRef } from '@/lib/ipc';
import { KIND_LABEL } from '@/lib/objects';
import { useWorkspace } from '@/stores/workspace';

import { SchemaChangeDialog } from './SchemaChangeDialog';

/** Whole-table actions from the explorer, on one object or a multi-selection. */
export type TableAction = 'drop' | 'truncate' | 'empty' | 'duplicate';

const VERB: Record<TableAction, string> = { drop: 'Drop', truncate: 'Truncate', empty: 'Empty', duplicate: 'Duplicate' };

/** `Truncate table · orders`, or `Truncate 3 tables` for a selection. */
export function tableActionTitle(action: TableAction, objects: ObjectRef[]): string {
  const [first] = objects;
  if (objects.length === 1 && first) return `${VERB[action]} ${KIND_LABEL[first.kind].toLowerCase()} · ${first.name}`;
  const allTables = objects.every((o) => o.kind === 'table');
  return `${VERB[action]} ${objects.length} ${allTables ? 'tables' : 'objects'}`;
}

/**
 * Builds the changes for a table action and hands them to the shared preview
 * → confirm → apply flow. The backend refuses what the engine cannot do and
 * asks for the destructive / production confirmations.
 */
export function TableActionDialog({ action, objects, onClose, onApplied }: { action: TableAction; objects: ObjectRef[]; onClose: () => void; onApplied: () => void }) {
  const session = useWorkspace((s) => s.session)!;
  const support = session.table_actions;
  const flavor = session.flavor;
  const [restartIdentity, setRestartIdentity] = useState(false);
  const [cascade, setCascade] = useState(false);
  const [disableForeignKeys, setDisableForeignKeys] = useState(false);
  const [newName, setNewName] = useState(`${objects[0]?.name ?? ''}_copy`);
  const [withData, setWithData] = useState(true);
  const dropCascade = flavor === 'postgres' || flavor === 'oracle' || flavor === 'duckdb' || flavor === 'snowflake' || flavor === 'vertica';

  const changes = useMemo<SchemaChange[] | string>(() => {
    const tables = objects.map(objectRef);
    switch (action) {
      case 'drop':
        return tables.map((table) => ({ op: 'drop_table', table, cascade }));
      case 'truncate':
        return tables.map((table) => ({ op: 'truncate_table', table, restart_identity: restartIdentity, cascade, disable_foreign_keys: disableForeignKeys }));
      case 'empty':
        return tables.map((table) => ({ op: 'empty_table', table }));
      case 'duplicate': {
        const [table] = tables;
        if (!table || !newName.trim()) return 'Enter the new name.';
        if (newName.trim() === table.name) return 'Choose a name other than the original’s.';
        return [{ op: 'duplicate_table', table, new_name: newName.trim(), with_data: withData }];
      }
    }
  }, [action, objects, cascade, restartIdentity, disableForeignKeys, newName, withData]);

  const note = action === 'truncate' ? support.truncate.note : action === 'empty' ? support.empty.note : action === 'duplicate' ? support.duplicate.note : null;
  const hasOptions = action === 'duplicate' || (action === 'drop' && dropCascade) || (action === 'truncate' && (support.truncate_restart_identity || support.truncate_cascade || support.truncate_disable_foreign_keys));

  return (
    <SchemaChangeDialog title={tableActionTitle(action, objects)} changes={changes} onClose={onClose} onApplied={onApplied} direct={!hasOptions && !note}>
      {objects.length > 1 && (
        <p className="ink-3" style={{ margin: '0 0 10px' }}>
          <span className="mono">{objects.map((o) => o.name).join(', ')}</span>
        </p>
      )}
      {action === 'duplicate' && (
        <>
          <TextField label="New table name" mono value={newName} onChange={(e) => setNewName(e.target.value)} autoFocus />
          <div style={{ marginTop: 10 }}>
            <Checkbox label="Copy the rows too (structure and data)" checked={withData} onChange={setWithData} />
          </div>
        </>
      )}
      {action === 'drop' && dropCascade && (
        <Checkbox
          label={flavor === 'oracle' ? 'CASCADE CONSTRAINTS (also drop foreign keys that reference them)' : 'CASCADE (also drop views and foreign keys that depend on them)'}
          checked={cascade}
          onChange={setCascade}
        />
      )}
      {action === 'truncate' && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
          {support.truncate_restart_identity && (
            <Checkbox label={flavor === 'sqlite' ? 'Reset AUTOINCREMENT counters' : 'RESTART IDENTITY (reset sequences owned by the columns)'} checked={restartIdentity} onChange={setRestartIdentity} />
          )}
          {support.truncate_cascade && <Checkbox label="CASCADE (also truncate tables whose foreign keys reference them)" checked={cascade} onChange={setCascade} />}
          {support.truncate_disable_foreign_keys && <Checkbox label="Turn off foreign key checks while truncating" checked={disableForeignKeys} onChange={setDisableForeignKeys} />}
        </div>
      )}
      {note && (
        <div style={{ marginTop: 12 }}>
          <Alert tone="info">{note}</Alert>
        </div>
      )}
    </SchemaChangeDialog>
  );
}
