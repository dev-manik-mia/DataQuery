import type { ReactNode } from 'react';

import type { DbError } from '@/bindings/DbError';

export function Alert({ tone = 'info', title, children }: { tone?: 'info' | 'positive' | 'caution' | 'critical'; title?: ReactNode; children?: ReactNode }) {
  return (
    <div className={`dq-alert dq-alert--${tone}`} role={tone === 'critical' ? 'alert' : 'status'}>
      {title && <div className="dq-alert__title">{title}</div>}
      {children}
    </div>
  );
}

/** A backend error with its code, hint and suggested action. */
export function ErrorAlert({ error, title }: { error: DbError; title?: ReactNode }) {
  return (
    <Alert tone="critical" title={title ?? error.message}>
      {title && <div>{error.message}</div>}
      {error.code && <div className="mono">{/^[0-9A-Z]{5}$/.test(error.code) ? 'SQLSTATE' : 'Error'} {error.code}</div>}
      {error.detail && <div>{error.detail}</div>}
      {error.hint && <div>Hint: {error.hint}</div>}
      {error.action && <div>{error.action}</div>}
    </Alert>
  );
}

export function EmptyState({ title, children, tone }: { title: ReactNode; children?: ReactNode; tone?: 'error' }) {
  return (
    <div className={`dq-state${tone ? ' dq-state--error' : ''}`}>
      <div className="dq-state__title">{title}</div>
      {children && <div className="dq-state__body">{children}</div>}
    </div>
  );
}
