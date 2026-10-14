import { memo } from 'react';

import type { Cell } from '@/bindings/Cell';
import { binaryLabel, localTime, preview, splitTemporal } from '@/lib/cells';

import styles from './Grid.module.css';

/** Renders one value according to the value rendering contract (§5.1). */
export const CellView = memo(function CellView({ cell }: { cell: Cell }) {
  switch (cell.kind) {
    case 'null':
      return <span className={styles.nullToken}>NULL</span>;
    case 'text':
      if (cell.value === '') return <span className={styles.emptyToken}>empty</span>;
      return (
        <span className={styles.cellText}>
          {preview(cell.value)}
          {cell.truncated && <span className={styles.muted} title="Value truncated in the grid; open the inspector for details"> …</span>}
        </span>
      );
    case 'number':
      return <span className={styles.cellText}>{cell.value}</span>;
    case 'bool':
      return <span>{String(cell.value)}</span>;
    case 'temporal': {
      if (!cell.zoned) return <span className={styles.cellText}>{cell.value}</span>;
      const { main, offset } = splitTemporal(cell.value);
      return (
        <span className={styles.cellText} title={localTime(cell.value) ?? undefined}>
          {main}
          <span className={styles.muted}>{offset}</span>
        </span>
      );
    }
    case 'json':
      return <span className={styles.cellText}>{preview(cell.value)}</span>;
    case 'binary':
      return <span className={`${styles.cellText} ${styles.muted}`}>{binaryLabel(cell.length, cell.head_hex)}</span>;
    case 'unsupported':
      return (
        <span className={`${styles.cellText} ${styles.unsupported}`} title={`${cell.type_name}: shown as text, read-only`}>
          {cell.value ?? `<${cell.type_name}>`}
        </span>
      );
  }
});
