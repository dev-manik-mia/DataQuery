import type { HTMLAttributes, ReactNode } from 'react';

interface CardProps extends Omit<HTMLAttributes<HTMLElement>, 'title'> {
  title?: ReactNode;
  actions?: ReactNode;
  footer?: ReactNode;
  flush?: boolean;
}

export function Card({ title, actions, footer, flush = false, className, children, ...rest }: CardProps) {
  return (
    <section className={['rb-card', className].filter(Boolean).join(' ')} {...rest}>
      {(title || actions) && (
        <header className="rb-card__head">
          {title && <h2 className="rb-card__title">{title}</h2>}
          {actions}
        </header>
      )}
      {flush ? children : <div className="rb-card__body">{children}</div>}
      {footer && <footer className="rb-card__foot">{footer}</footer>}
    </section>
  );
}

export function PageHead({ eyebrow, title, actions, mono }: { eyebrow: ReactNode; title: ReactNode; actions?: ReactNode; mono?: boolean }) {
  return (
    <div className="rb-pagehead">
      <div className="rb-pagehead__text">
        <div className="rb-eyebrow">{eyebrow}</div>
        <h1 className={`rb-pagehead__title${mono ? ' mono' : ''}`}>{title}</h1>
      </div>
      {actions && <div className="rb-pagehead__actions">{actions}</div>}
    </div>
  );
}

export function Eyebrow({ children }: { children: ReactNode }) {
  return <div className="rb-eyebrow">{children}</div>;
}
