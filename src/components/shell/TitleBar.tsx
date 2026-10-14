import type { ReactNode } from 'react';

import { isMac } from '@/lib/platform';

import styles from './Shell.module.css';

interface TitleBarProps {
  onBrand?: () => void;
  crumbs?: ReactNode;
  center?: ReactNode;
  actions?: ReactNode;
  production?: boolean;
}

/** The 40px custom title bar. Empty areas drag the window. */
export function TitleBar({ onBrand, crumbs, center, actions, production }: TitleBarProps) {
  const classes = [styles.titlebar, isMac && styles.titlebarMac, production && styles.titlebarProduction].filter(Boolean).join(' ');
  return (
    <header className={classes} data-tauri-drag-region>
      <button type="button" className={styles.brand} onClick={onBrand} aria-label="DataQuery — connections">
        <span className={styles.brandMark} aria-hidden />
        DataQuery
      </button>
      {crumbs}
      {center ? (
        <div className={styles.center} data-tauri-drag-region>
          {center}
        </div>
      ) : (
        <div className={styles.spacer} data-tauri-drag-region />
      )}
      {actions && <div className={styles.actions}>{actions}</div>}
    </header>
  );
}

/** `/ Connections / New connection` style breadcrumbs. */
export function Crumbs({ items }: { items: { label: string; onClick?: () => void }[] }) {
  return (
    <nav className={styles.crumbs} aria-label="Breadcrumb">
      {items.map((item, i) => (
        <span key={item.label} className={styles.crumbs}>
          <span className={styles.crumbSep} aria-hidden>
            /
          </span>
          {i === items.length - 1 ? (
            <span className={styles.crumbCurrent} aria-current="page">
              {item.label}
            </span>
          ) : (
            <button type="button" className={styles.crumbLink} onClick={item.onClick}>
              {item.label}
            </button>
          )}
        </span>
      ))}
    </nav>
  );
}

export function TitleDivider() {
  return <span className={styles.divider} aria-hidden />;
}
