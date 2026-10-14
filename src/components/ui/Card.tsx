import type { HTMLAttributes, ReactNode } from 'react';

interface CardProps extends Omit<HTMLAttributes<HTMLElement>, 'title'> {
  title?: ReactNode;
  actions?: ReactNode;
  footer?: ReactNode;
  flush?: boolean;
}

export function Card({ title, actions, footer, flush = false, className, children, ...rest }: CardProps) {
  return (
    <section className={['dq-card', className].filter(Boolean).join(' ')} {...rest}>
      {(title || actions) && (
        <header className="dq-card__head">
          {title && <h2 className="dq-card__title">{title}</h2>}
          {actions}
        </header>
      )}
      {flush ? children : <div className="dq-card__body">{children}</div>}
      {footer && <footer className="dq-card__foot">{footer}</footer>}
    </section>
  );
}

export function PageHead({ eyebrow, title, actions, mono }: { eyebrow: ReactNode; title: ReactNode; actions?: ReactNode; mono?: boolean }) {
  return (
    <div className="dq-pagehead">
      <div className="dq-pagehead__text">
        <div className="dq-eyebrow">{eyebrow}</div>
        <h1 className={`dq-pagehead__title${mono ? ' mono' : ''}`}>{title}</h1>
      </div>
      {actions && <div className="dq-pagehead__actions">{actions}</div>}
    </div>
  );
}

export function Eyebrow({ children }: { children: ReactNode }) {
  return <div className="dq-eyebrow">{children}</div>;
}
