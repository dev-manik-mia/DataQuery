import { useMemo, useState } from 'react';

import type { ColumnDef } from '@/bindings/ColumnDef';
import type { ColumnInfo } from '@/bindings/ColumnInfo';
import type { ObjectRef } from '@/bindings/ObjectRef';
import type { SchemaChange } from '@/bindings/SchemaChange';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import type { TableStructure } from '@/bindings/TableStructure';
import { Button, Checkbox, IconButton, Icon, SelectField, TextField } from '@/components/ui';
import { hasSchemas } from '@/lib/engines';

import { SchemaChangeDialog } from './SchemaChangeDialog';

/** Common column types, offered as suggestions (any type can be typed). */
export const TYPE_SUGGESTIONS: Record<SqlFlavor, string[]> = {
  postgres: ['bigint', 'integer', 'smallint', 'numeric(12,2)', 'double precision', 'boolean', 'text', 'varchar(255)', 'uuid', 'date', 'timestamp', 'timestamptz', 'jsonb', 'bytea', 'text[]', 'inet', 'interval'],
  mysql: ['bigint', 'int', 'smallint', 'tinyint(1)', 'decimal(12,2)', 'double', 'varchar(255)', 'text', 'char(36)', 'date', 'datetime', 'timestamp', 'json', 'blob', "enum('a','b')"],
  sqlserver: ['bigint', 'int', 'smallint', 'bit', 'decimal(12,2)', 'float', 'money', 'nvarchar(255)', 'nvarchar(max)', 'varchar(255)', 'uniqueidentifier', 'date', 'datetime2(3)', 'datetimeoffset(3)', 'time', 'varbinary(max)'],
  clickhouse: ['UInt64', 'Int64', 'UInt32', 'Int32', 'Float64', 'Decimal(12, 2)', 'String', 'LowCardinality(String)', 'Bool', 'Date', 'DateTime', 'DateTime64(3)', 'UUID', 'Array(String)', 'Map(String, String)', 'JSON'],
  duckdb: ['BIGINT', 'INTEGER', 'SMALLINT', 'DECIMAL(12,2)', 'DOUBLE', 'BOOLEAN', 'VARCHAR', 'UUID', 'DATE', 'TIMESTAMP', 'TIMESTAMPTZ', 'INTERVAL', 'BLOB', 'JSON', 'VARCHAR[]', 'STRUCT(a INTEGER)'],
  snowflake: ['NUMBER(38,0)', 'NUMBER(12,2)', 'FLOAT', 'BOOLEAN', 'VARCHAR', 'VARCHAR(255)', 'DATE', 'TIME', 'TIMESTAMP_NTZ', 'TIMESTAMP_TZ', 'VARIANT', 'OBJECT', 'ARRAY', 'BINARY', 'GEOGRAPHY'],
  oracle: ['NUMBER(19)', 'NUMBER(10)', 'NUMBER(12,2)', 'BINARY_DOUBLE', 'VARCHAR2(255 CHAR)', 'NVARCHAR2(255)', 'CHAR(1)', 'CLOB', 'DATE', 'TIMESTAMP(6)', 'TIMESTAMP(6) WITH TIME ZONE', 'RAW(16)', 'BLOB', 'JSON'],
  // Document engines have no schema forms.
  mongodb: [],
  partiql: [],
};

/** What the engine calls a column that numbers itself. */
const IDENTITY_LABEL: Record<SqlFlavor, string> = {
  postgres: 'Identity',
  mysql: 'AUTO_INCREMENT',
  sqlserver: 'IDENTITY',
  oracle: 'Identity',
  clickhouse: 'Identity (not supported)',
  duckdb: 'Identity (use a sequence default)',
  mongodb: 'Identity (not supported)',
  partiql: 'Identity (not supported)',
  snowflake: 'IDENTITY',
};

function TypeList({ flavor }: { flavor: SqlFlavor }) {
  return (
    <datalist id={`dq-types-${flavor}`}>
      {TYPE_SUGGESTIONS[flavor].map((t) => (
        <option key={t} value={t} />
      ))}
    </datalist>
  );
}

function emptyColumn(): ColumnDef & { pk: boolean } {
  return { name: '', data_type: '', nullable: true, default: null, identity: false, pk: false };
}

/** Name of a new object in the same container as `container`. */
function sibling(container: ObjectRef, name: string): ObjectRef {
  return { kind: 'table', database: container.database, schema: container.schema, name };
}

// ------------------------------------------------------------------ create table

export function CreateTableDialog({ container, flavor, onClose, onApplied }: { container: ObjectRef; flavor: SqlFlavor; onClose: () => void; onApplied: (table: ObjectRef) => void }) {
  const [name, setName] = useState('');
  const [columns, setColumns] = useState([{ ...emptyColumn(), name: 'id', data_type: 'bigint', nullable: false, identity: true, pk: true }]);
  const patch = (i: number, p: Partial<(typeof columns)[number]>) => setColumns((cs) => cs.map((c, j) => (j === i ? { ...c, ...p } : c)));
  const table = sibling(container, name.trim());
  const changes = useMemo<SchemaChange[] | string>(() => {
    if (!name.trim()) return 'Name the table.';
    if (columns.some((c) => !c.name.trim() || !c.data_type.trim())) return 'Every column needs a name and a type.';
    return [
      {
        op: 'create_table',
        table,
        columns: columns.map((c) => ({ name: c.name.trim(), data_type: c.data_type.trim(), nullable: c.nullable && !c.pk, default: c.default?.trim() || null, identity: c.identity })),
        primary_key: columns.filter((c) => c.pk).map((c) => c.name.trim()),
      },
    ];
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [name, columns]);
  return (
    <SchemaChangeDialog title={`Create table in ${container.schema ?? container.database}`} changes={changes} onClose={onClose} onApplied={() => onApplied(table)}>
      <TypeList flavor={flavor} />
      <TextField label="Table name" mono value={name} onChange={(e) => setName(e.target.value)} autoFocus />
      <div style={{ display: 'grid', gridTemplateColumns: '1.2fr 1.2fr 1fr auto auto auto auto', gap: '6px 8px', alignItems: 'end', marginTop: 12 }}>
        <span className="dq-field__label">Column</span>
        <span className="dq-field__label">Type</span>
        <span className="dq-field__label">Default</span>
        <span className="dq-field__label">Null</span>
        <span className="dq-field__label">PK</span>
        <span className="dq-field__label">{flavor === 'mysql' ? 'Auto inc.' : IDENTITY_LABEL[flavor]}</span>
        <span />
        {columns.map((c, i) => (
          <div key={i} style={{ display: 'contents' }}>
            <input className="dq-input dq-input--mono dq-input--sm" aria-label="Column name" value={c.name} onChange={(e) => patch(i, { name: e.target.value })} />
            <input className="dq-input dq-input--mono dq-input--sm" aria-label="Column type" list={`dq-types-${flavor}`} value={c.data_type} onChange={(e) => patch(i, { data_type: e.target.value })} />
            <input className="dq-input dq-input--mono dq-input--sm" aria-label="Default expression" placeholder="none" value={c.default ?? ''} onChange={(e) => patch(i, { default: e.target.value })} />
            <input type="checkbox" aria-label="Nullable" checked={c.nullable && !c.pk} disabled={c.pk} onChange={(e) => patch(i, { nullable: e.target.checked })} />
            <input type="checkbox" aria-label="Primary key" checked={c.pk} onChange={(e) => patch(i, { pk: e.target.checked })} />
            <input type="checkbox" aria-label="Identity" checked={c.identity} onChange={(e) => patch(i, { identity: e.target.checked })} />
            <IconButton label="Remove column" disabled={columns.length === 1} onClick={() => setColumns((cs) => cs.filter((_, j) => j !== i))}>
              <Icon name="minus" />
            </IconButton>
          </div>
        ))}
      </div>
      <Button size="sm" style={{ marginTop: 10 }} onClick={() => setColumns((cs) => [...cs, emptyColumn()])}>
        Add column
      </Button>
    </SchemaChangeDialog>
  );
}

// ------------------------------------------------------------------ table edits

/** Every form on the Structure tab. */
export type StructureEdit =
  | { kind: 'add-column' }
  | { kind: 'alter-column'; column: ColumnInfo }
  | { kind: 'rename-column'; column: ColumnInfo }
  | { kind: 'drop-column'; column: ColumnInfo }
  | { kind: 'rename-table' }
  | { kind: 'drop-table' }
  | { kind: 'add-index' }
  | { kind: 'drop-index'; name: string }
  | { kind: 'add-foreign-key' }
  | { kind: 'add-unique' }
  | { kind: 'add-check' }
  | { kind: 'drop-constraint'; name: string; constraint: 'primary_key' | 'foreign_key' | 'unique' | 'check' | 'exclusion' };

const FK_ACTIONS = ['', 'CASCADE', 'SET NULL', 'SET DEFAULT', 'RESTRICT', 'NO ACTION'];

function ColumnPicker({ structure, value, onChange, label }: { structure: TableStructure; value: string[]; onChange: (v: string[]) => void; label: string }) {
  return (
    <fieldset style={{ border: 'none', padding: 0, margin: '10px 0 0' }}>
      <legend className="dq-field__label">{label}</legend>
      <div style={{ display: 'flex', flexWrap: 'wrap', gap: '4px 14px' }}>
        {structure.columns.map((c) => (
          <Checkbox key={c.name} label={<span className="mono">{c.name}</span>} checked={value.includes(c.name)} onChange={(on) => onChange(on ? [...value, c.name] : value.filter((v) => v !== c.name))} />
        ))}
      </div>
      {value.length > 1 && <span className="dq-field__hint">Order: {value.join(', ')}</span>}
    </fieldset>
  );
}

export function StructureEditDialog({ edit, structure, flavor, onClose, onApplied }: { edit: StructureEdit; structure: TableStructure; flavor: SqlFlavor; onClose: () => void; onApplied: () => void }) {
  const table = structure.object;
  const base = edit.kind === 'alter-column' ? edit.column : null;
  const [name, setName] = useState(edit.kind === 'rename-column' || edit.kind === 'alter-column' ? edit.column.name : edit.kind === 'rename-table' ? table.name : '');
  const [dataType, setDataType] = useState(base?.data_type ?? '');
  const [nullable, setNullable] = useState(base?.nullable ?? true);
  const [defaultExpr, setDefault] = useState(base?.default ?? '');
  const [identity, setIdentity] = useState(false);
  const [columns, setColumns] = useState<string[]>([]);
  const [descending, setDescending] = useState(false);
  const [unique, setUnique] = useState(false);
  const [target, setTarget] = useState('');
  const [targetColumns, setTargetColumns] = useState('');
  const [onDelete, setOnDelete] = useState('');
  const [onUpdate, setOnUpdate] = useState('');
  const [expression, setExpression] = useState('');
  const [cascade, setCascade] = useState(false);

  const parseTarget = (): ObjectRef | null => {
    const parts = target.trim().split('.');
    if (!target.trim() || parts.length > 2) return null;
    const [first, second] = parts;
    return second ? { kind: 'table', database: table.database, schema: hasSchemas(flavor) ? first! : null, name: second } : { kind: 'table', database: table.database, schema: table.schema, name: first! };
  };

  const changes: SchemaChange[] | string = (() => {
    switch (edit.kind) {
      case 'add-column':
        if (!name.trim() || !dataType.trim()) return 'Enter a name and a type.';
        return [{ op: 'add_column', table, column: { name: name.trim(), data_type: dataType.trim(), nullable, default: defaultExpr.trim() || null, identity } }];
      case 'alter-column': {
        const c = edit.column;
        const typeChanged = dataType.trim() !== c.data_type;
        const nullChanged = nullable !== c.nullable;
        const defaultChanged = (defaultExpr.trim() || null) !== (c.default ?? null);
        const renamed = name.trim() !== c.name;
        const out: SchemaChange[] = [];
        if (typeChanged || nullChanged || defaultChanged) {
          out.push({
            op: 'alter_column',
            table,
            column: c.name,
            data_type: typeChanged ? dataType.trim() : null,
            nullable: nullChanged ? nullable : null,
            default: defaultChanged ? (defaultExpr.trim() ? { action: 'set', expression: defaultExpr.trim() } : { action: 'drop' }) : null,
          });
        }
        if (renamed) out.push({ op: 'rename_column', table, column: c.name, new_name: name.trim() });
        return out.length ? out : 'Change something first.';
      }
      case 'rename-column':
        return name.trim() && name.trim() !== edit.column.name ? [{ op: 'rename_column', table, column: edit.column.name, new_name: name.trim() }] : 'Enter the new name.';
      case 'drop-column':
        return [{ op: 'drop_column', table, column: edit.column.name }];
      case 'rename-table':
        return name.trim() && name.trim() !== table.name ? [{ op: 'rename_table', table, new_name: name.trim() }] : 'Enter the new name.';
      case 'drop-table':
        return [{ op: 'drop_table', table, cascade }];
      case 'add-index':
        if (!name.trim() || !columns.length) return 'Name the index and choose its columns.';
        return [{ op: 'create_index', table, name: name.trim(), columns: columns.map((c) => ({ name: c, descending })), unique }];
      case 'drop-index':
        return [{ op: 'drop_index', table, name: edit.name }];
      case 'add-foreign-key': {
        const t = parseTarget();
        const tcols = targetColumns.split(',').map((c) => c.trim()).filter(Boolean);
        if (!name.trim() || !columns.length || !t || tcols.length !== columns.length) return 'Name the key, choose columns, and give the referenced table and as many referenced columns.';
        return [{ op: 'add_foreign_key', table, name: name.trim(), columns, target: t, target_columns: tcols, on_delete: onDelete || null, on_update: onUpdate || null }];
      }
      case 'add-unique':
        return name.trim() && columns.length ? [{ op: 'add_unique', table, name: name.trim(), columns }] : 'Name the constraint and choose its columns.';
      case 'add-check':
        return name.trim() && expression.trim() ? [{ op: 'add_check', table, name: name.trim(), expression: expression.trim() }] : 'Name the constraint and enter the condition.';
      case 'drop-constraint':
        return [{ op: 'drop_constraint', table, name: edit.name, kind: edit.constraint }];
    }
  })();

  const titles: Record<StructureEdit['kind'], string> = {
    'add-column': 'Add column',
    'alter-column': 'Edit column',
    'rename-column': 'Rename column',
    'drop-column': 'Drop column',
    'rename-table': 'Rename table',
    'drop-table': 'Drop table',
    'add-index': 'Create index',
    'drop-index': 'Drop index',
    'add-foreign-key': 'Add foreign key',
    'add-unique': 'Add unique constraint',
    'add-check': 'Add check constraint',
    'drop-constraint': 'Drop constraint',
  };
  const direct = ['drop-column', 'drop-index', 'drop-constraint'].includes(edit.kind);
  const nameField = (label: string) => <TextField label={label} mono value={name} onChange={(e) => setName(e.target.value)} autoFocus />;

  return (
    <SchemaChangeDialog title={`${titles[edit.kind]} · ${table.name}`} changes={changes} onClose={onClose} onApplied={onApplied} direct={direct}>
      <TypeList flavor={flavor} />
      {(edit.kind === 'add-column' || edit.kind === 'alter-column') && (
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
          {nameField('Column name')}
          <TextField label="Type" mono list={`dq-types-${flavor}`} value={dataType} onChange={(e) => setDataType(e.target.value)} />
          <TextField label="Default" mono placeholder="none" hint="A SQL expression, e.g. 'pending' or now()." value={defaultExpr} onChange={(e) => setDefault(e.target.value)} />
          <div style={{ display: 'flex', flexDirection: 'column', gap: 8, paddingTop: 20 }}>
            <Checkbox label="Allows NULL" checked={nullable} onChange={setNullable} />
            {edit.kind === 'add-column' && <Checkbox label={IDENTITY_LABEL[flavor]} checked={identity} onChange={setIdentity} />}
          </div>
        </div>
      )}
      {(edit.kind === 'rename-column' || edit.kind === 'rename-table') && nameField('New name')}
      {edit.kind === 'drop-table' && flavor === 'postgres' && <Checkbox label="CASCADE (also drop views and foreign keys that depend on it)" checked={cascade} onChange={setCascade} />}
      {edit.kind === 'drop-table' && flavor === 'oracle' && <Checkbox label="CASCADE CONSTRAINTS (also drop foreign keys that reference it)" checked={cascade} onChange={setCascade} />}
      {edit.kind === 'add-index' && (
        <>
          {nameField('Index name')}
          <ColumnPicker structure={structure} value={columns} onChange={setColumns} label="Columns" />
          <div style={{ display: 'flex', gap: 16, marginTop: 10 }}>
            <Checkbox label="Unique" checked={unique} onChange={setUnique} />
            <Checkbox label="Descending" checked={descending} onChange={setDescending} />
          </div>
        </>
      )}
      {edit.kind === 'add-foreign-key' && (
        <>
          {nameField('Constraint name')}
          <ColumnPicker structure={structure} value={columns} onChange={setColumns} label="Columns" />
          <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12, marginTop: 10 }}>
            <TextField label="Referenced table" mono placeholder={hasSchemas(flavor) ? 'schema.table' : 'table'} value={target} onChange={(e) => setTarget(e.target.value)} />
            <TextField label="Referenced columns" mono placeholder="id" hint="Comma-separated, in the same order." value={targetColumns} onChange={(e) => setTargetColumns(e.target.value)} />
            <SelectField label="On delete" value={onDelete} onChange={(e) => setOnDelete(e.target.value)} options={FK_ACTIONS.map((a) => ({ value: a, label: a || 'Default (no action)' }))} />
            <SelectField label="On update" value={onUpdate} onChange={(e) => setOnUpdate(e.target.value)} options={FK_ACTIONS.map((a) => ({ value: a, label: a || 'Default (no action)' }))} />
          </div>
        </>
      )}
      {edit.kind === 'add-unique' && (
        <>
          {nameField('Constraint name')}
          <ColumnPicker structure={structure} value={columns} onChange={setColumns} label="Columns" />
        </>
      )}
      {edit.kind === 'add-check' && (
        <>
          {nameField('Constraint name')}
          <TextField label="Condition" mono placeholder="total >= 0" value={expression} onChange={(e) => setExpression(e.target.value)} />
        </>
      )}
    </SchemaChangeDialog>
  );
}
