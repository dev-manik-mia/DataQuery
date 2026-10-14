import type { ReactNode } from 'react';

import styles from './Shell.module.css';

export type ConnectionStatus = 'connected' | 'connecting' | 'failed' | 'disconnected';

const statusTone: Record<ConnectionStatus, string> = {
  connected: 'var(--positive)',
  connecting: 'var(--caution)',
  failed: 'var(--critical)',
  disconnected: 'var(--ink-3)',
};

const statusLabel: Record<ConnectionStatus, string> = {
  connected: 'Connected',
  connecting: 'Connecting',
  failed: 'Failed',
  disconnected: 'Disconnected',
};

export function StatusBar({ status, left, right }: { status: ConnectionStatus; left?: ReactNode; right?: ReactNode }) {
  return (
    <footer className={styles.statusbar} aria-label="Status">
      <span className={styles.status} style={{ color: statusTone[status] }} role="status">
        <span className={styles.statusDot} aria-hidden />
        {statusLabel[status]}
      </span>
      {left}
      <span style={{ flex: 1 }} />
      {right}
    </footer>
  );
}
