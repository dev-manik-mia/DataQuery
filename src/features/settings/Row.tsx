import type { ReactNode } from 'react';

import styles from './Settings.module.css';

/** One settings line: title and help on the left, its control on the right. */
export function Row({ title, help, children }: { title: ReactNode; help?: ReactNode; children?: ReactNode }) {
  return (
    <div className={styles.row}>
      <div className={styles.rowText}>
        <span className={styles.rowTitle}>{title}</span>
        {help && <span className={styles.rowHelp}>{help}</span>}
      </div>
      {children}
    </div>
  );
}
