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

/**
 * Coloured dot plus word, so the state never relies on colour alone. Only one
 * copy on screen should be `live`, or screen readers announce changes twice.
 */
export function StatusIndicator({ status, live = true }: { status: ConnectionStatus; live?: boolean }) {
  return (
    <span className={styles.status} style={{ color: statusTone[status] }} role={live ? 'status' : undefined}>
      <span className={styles.statusDot} aria-hidden />
      {statusLabel[status]}
    </span>
  );
}

export function StatusBar({ status, left, right }: { status: ConnectionStatus; left?: ReactNode; right?: ReactNode }) {
  return (
    <footer className={styles.statusbar} aria-label="Status">
      <StatusIndicator status={status} />
      {left}
      <span style={{ flex: 1 }} />
      {right}
    </footer>
  );
}
