import { useState } from 'react';

import type { DocField } from '@/bindings/DocField';
import type { Document } from '@/bindings/Document';
import { Icon, IconButton } from '@/components/ui';
import { cellText, preview } from '@/lib/cells';
import { formatBytes, formatCount, plural } from '@/lib/format';

import styles from './Documents.module.css';

const CONTAINERS = new Set(['Object', 'Array', 'M', 'L']);

/** Scalar text as the shell shows it: strings quoted, null as `null`. */
function scalar(field: DocField): string {
  const text = cellText(field.value);
  if (text === null) return 'null';
  if (field.value.kind === 'text' && (field.type_label === 'String' || field.type_label === 'S')) return JSON.stringify(text);
  if (field.value.kind === 'binary') return `${formatBytes(field.value.length)} · ${field.value.head_hex.slice(0, 16)}…`;
  return preview(text, 300);
}

function summary(field: DocField): string {
  const n = field.children.length + field.more;
  return field.type_label === 'Array' || field.type_label === 'L' ? `[ ${plural(n, 'item')} ]` : `{ ${plural(n, 'field')} }`;
}

function Field({ field, depth }: { field: DocField; depth: number }) {
  const container = CONTAINERS.has(field.type_label) || field.children.length > 0;
  const [open, setOpen] = useState(depth === 0 && field.children.length <= 8);
  return (
    <>
      <div className={styles.line} style={{ paddingLeft: 12 + depth * 18 }}>
        <button
          type="button"
          className={styles.toggle}
          disabled={!container || field.children.length === 0}
          aria-expanded={container ? open : undefined}
          aria-label={container ? `${open ? 'Collapse' : 'Expand'} ${field.key}` : undefined}
          onClick={() => setOpen(!open)}
        >
          {container && field.children.length > 0 ? (open ? '▾' : '▸') : ''}
        </button>
        <span className={styles.key}>{field.key}</span>
        <span className={styles.sep}>:</span>
        <span className={styles.value} data-kind={container ? 'container' : field.value.kind}>
          {container ? summary(field) : scalar(field)}
        </span>
        <span className={styles.type}>{field.type_label}</span>
      </div>
      {container &&
        open &&
        field.children.map((c, i) => <Field key={`${c.key}:${i}`} field={c} depth={depth + 1} />)}
      {container && open && field.more > 0 && (
        <div className={styles.line} style={{ paddingLeft: 32 + (depth + 1) * 18 }}>
          <span className={styles.more}>… {formatCount(field.more)} more</span>
        </div>
      )}
    </>
  );
}

/** Documents as cards with typed, collapsible fields (screen 13, Tree). */
export function DocumentTree({
  documents,
  onEdit,
  onDelete,
  onCopy,
}: {
  documents: Document[];
  onEdit?: (d: Document) => void;
  onDelete?: (d: Document) => void;
  onCopy: (d: Document) => void;
}) {
  return (
    <div className={styles.cards}>
      {documents.map((d, i) => (
        <article key={d.key || i} className={styles.card} aria-label={d.label}>
          <div className={styles.cardHead}>
            <span className={styles.label}>{d.label || `Document ${i + 1}`}</span>
            {d.size_bytes !== null && <span className={styles.size}>{formatBytes(d.size_bytes)}</span>}
            <IconButton label={`Copy ${d.label}`} bare onClick={() => onCopy(d)}>
              <Icon name="copy" size={12} />
            </IconButton>
            {onEdit && (
              <IconButton label={d.truncated ? 'Too large to edit here' : `Edit ${d.label}`} bare disabled={d.truncated} onClick={() => onEdit(d)}>
                <Icon name="edit" size={12} />
              </IconButton>
            )}
            {onDelete && (
              <IconButton label={`Delete ${d.label}`} bare onClick={() => onDelete(d)}>
                <Icon name="trash" size={12} />
              </IconButton>
            )}
          </div>
          <div className={styles.lines}>
            {d.fields.map((f) => (
              <Field key={f.key} field={f} depth={0} />
            ))}
          </div>
        </article>
      ))}
    </div>
  );
}
