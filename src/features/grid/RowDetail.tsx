import { useRef, useState, type KeyboardEvent } from 'react';

import type { Cell } from '@/bindings/Cell';
import { Icon, IconButton } from '@/components/ui';
import { cellText, isTruncated } from '@/lib/cells';

import type { ForeignKeyLink, GridColumn, GridRow } from './DataGrid';
import styles from './Grid.module.css';

interface RowDetailProps {
  columns: GridColumn[];
  row: GridRow;
  rowIndex: number;
  /** Row number shown in the title (1-based, paging included). */
  number: number;
  /** Columns in on-screen order (hidden ones left out). */
  order: number[];
  /** The cell as it should be shown (masking applied). */
  display: (col: number) => Cell;
  masked: ReadonlySet<number>;
  foreignKeys?: ReadonlyMap<number, ForeignKeyLink>;
  activeCol: number | null;
  /** Present when the row's cells can be edited (pending changes, saved with the grid's Save). */
  canEdit: (col: number) => boolean;
  onEdit: (col: number, value: string | null) => void;
  onInspect: (col: number) => void;
  onFocusColumn: (col: number) => void;
  onClose: () => void;
}

/** The active row as a form: every column with its type and value; Space toggles it (quick look). */
export function RowDetail({
  columns,
  row,
  rowIndex,
  number,
  order,
  display,
  masked,
  foreignKeys,
  activeCol,
  canEdit,
  onEdit,
  onInspect,
  onFocusColumn,
  onClose,
}: RowDetailProps) {
  const [filter, setFilter] = useState('');
  const needle = filter.trim().toLowerCase();
  const shown = needle ? order.filter((c) => columns[c]?.name.toLowerCase().includes(needle)) : order;

  return (
    <aside className={styles.detail} aria-label={`Row ${number} details`}>
      <header className={styles.detailHead}>
        <span className={styles.detailTitle}>
          Row {number}
          {row.status === 'new' ? ' · new' : row.status === 'deleted' ? ' · deleted' : ''}
        </span>
        <IconButton label="Close row details (Space)" onClick={onClose}>
          <Icon name="close" size={12} />
        </IconButton>
      </header>
      <input
        className="dq-input dq-input--sm"
        aria-label="Filter columns"
        placeholder={`Filter ${order.length} columns`}
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
        spellCheck={false}
      />
      <div className={styles.detailFields}>
        {shown.map((col) => {
          const column = columns[col]!;
          const cell = display(col);
          // Keyed by the row so a draft never carries over to the next row.
          return (
            <DetailField
              key={`${row.key}:${col}`}
              column={column}
              cell={cell}
              active={activeCol === col}
              edited={row.edited?.has(col) ?? false}
              editable={
                canEdit(col) &&
                !masked.has(col) &&
                cell.kind !== 'binary' &&
                cell.kind !== 'unsupported' &&
                !isTruncated(cell)
              }
              foreignKey={cell.kind !== 'null' ? foreignKeys?.get(col) : undefined}
              onEdit={(value) => onEdit(col, value)}
              onInspect={() => onInspect(col)}
              onFocus={() => onFocusColumn(col)}
              onFollow={() => foreignKeys?.get(col)?.open(rowIndex)}
            />
          );
        })}
        {!shown.length && <div className={styles.empty}>No column matches.</div>}
      </div>
    </aside>
  );
}

interface DetailFieldProps {
  column: GridColumn;
  cell: Cell;
  active: boolean;
  edited: boolean;
  editable: boolean;
  foreignKey?: ForeignKeyLink;
  onEdit: (value: string | null) => void;
  onInspect: () => void;
  onFocus: () => void;
  onFollow: () => void;
}

function DetailField({
  column,
  cell,
  active,
  edited,
  editable,
  foreignKey,
  onEdit,
  onInspect,
  onFocus,
  onFollow,
}: DetailFieldProps) {
  const text = cellText(cell);
  const [draft, setDraft] = useState<string | null>(null);
  const multiline = (text?.length ?? 0) > 60 || (text?.includes('\n') ?? false);
  const cancelled = useRef(false);
  const commit = () => {
    if (!cancelled.current && draft !== null && draft !== (text ?? '')) onEdit(draft);
    cancelled.current = false;
    setDraft(null);
  };
  /** Enter (single line) saves, Escape drops the draft; both leave the field. */
  const onKeyDown = (e: KeyboardEvent<HTMLInputElement | HTMLTextAreaElement>) => {
    e.stopPropagation();
    if (e.key === 'Escape' || (e.key === 'Enter' && !multiline)) {
      cancelled.current = e.key === 'Escape';
      e.currentTarget.blur();
    }
  };
  const value = draft ?? text ?? '';
  const common = {
    className: `dq-input dq-input--sm dq-input--mono ${styles.detailInput}`,
    'aria-label': column.name,
    value,
    placeholder: text === null ? 'NULL' : undefined,
    readOnly: !editable,
    spellCheck: false,
    onFocus,
    onChange: (e: { target: { value: string } }) => setDraft(e.target.value),
    onBlur: commit,
    onKeyDown,
  };

  return (
    <div className={styles.detailField} data-active={active} data-edited={edited}>
      <div className={styles.detailLabel}>
        <span className="ellipsis mono">{column.name}</span>
        {column.badge && <span className={styles.headBadge}>{column.badge}</span>}
        <span className={`${styles.detailType} ellipsis`}>{column.typeName}</span>
        <span style={{ flex: 1 }} />
        {foreignKey && (
          <IconButton label={foreignKey.title} onClick={onFollow}>
            <Icon name="link" size={11} />
          </IconButton>
        )}
        {editable && text !== null && (
          <IconButton label={`Set ${column.name} to NULL`} onClick={() => onEdit(null)}>
            <Icon name="minus" size={11} />
          </IconButton>
        )}
        <IconButton label={`Inspect ${column.name}`} onClick={onInspect}>
          <Icon name="external" size={11} />
        </IconButton>
      </div>
      {multiline ? (
        <textarea {...common} rows={Math.min(8, Math.max(3, value.split('\n').length))} />
      ) : (
        <input {...common} />
      )}
    </div>
  );
}
