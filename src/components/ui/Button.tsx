import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from 'react';

import { Spinner } from './Spinner';

export type ButtonVariant = 'primary' | 'ghost' | 'danger' | 'caution' | 'plain';
export type ButtonSize = 'xs' | 'sm' | 'md';

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  busy?: boolean;
  /** Keyboard hint shown after the label, e.g. `⌘↵`. */
  shortcut?: string;
  icon?: ReactNode;
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  { variant = 'ghost', size = 'md', busy = false, shortcut, icon, className, children, disabled, type = 'button', ...rest },
  ref,
) {
  const classes = ['dq-btn', `dq-btn--${variant}`, size !== 'md' && `dq-btn--${size}`, className].filter(Boolean).join(' ');
  return (
    <button ref={ref} type={type} className={classes} disabled={disabled || busy} aria-busy={busy || undefined} {...rest}>
      {busy ? <Spinner current /> : icon}
      {children}
      {shortcut && <span className="dq-btn__kbd">{shortcut}</span>}
    </button>
  );
});

interface IconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  /** Accessible name; icon buttons have no visible text. */
  label: string;
  bare?: boolean;
}

export function IconButton({ label, bare = false, className, children, type = 'button', ...rest }: IconButtonProps) {
  return (
    <button
      type={type}
      aria-label={label}
      title={label}
      className={['dq-icon-btn', bare && 'dq-icon-btn--bare', className].filter(Boolean).join(' ')}
      {...rest}
    >
      {children}
    </button>
  );
}
