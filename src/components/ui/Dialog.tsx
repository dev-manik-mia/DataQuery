import { useEffect, useRef, type ReactNode } from 'react';
import { createPortal } from 'react-dom';

interface DialogProps {
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  wide?: boolean;
  /** Adds the 3px critical rule used for production and destructive prompts. */
  critical?: boolean;
  /** Places the dialog near the top (command palette). */
  top?: boolean;
  /** Element to focus first; defaults to the first focusable control. */
  initialFocus?: React.RefObject<HTMLElement | null>;
  className?: string;
}

const FOCUSABLE = 'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

/** Modal dialog: traps focus, closes on Escape and restores focus on close. */
export function Dialog({ title, onClose, children, footer, wide, critical, top, initialFocus, className }: DialogProps) {
  const ref = useRef<HTMLDivElement>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    const target = initialFocus?.current ?? ref.current?.querySelector<HTMLElement>(FOCUSABLE);
    target?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        onCloseRef.current();
      } else if (e.key === 'Tab' && ref.current) {
        const items = [...ref.current.querySelectorAll<HTMLElement>(FOCUSABLE)];
        const first = items[0];
        const last = items[items.length - 1];
        if (e.shiftKey && document.activeElement === first) {
          e.preventDefault();
          last?.focus();
        } else if (!e.shiftKey && document.activeElement === last) {
          e.preventDefault();
          first?.focus();
        }
      }
    };
    document.addEventListener('keydown', onKey, true);
    return () => {
      document.removeEventListener('keydown', onKey, true);
      previous?.focus?.();
    };
  }, [initialFocus]);

  return createPortal(
    <div className={`rb-scrim${top ? ' rb-scrim--top' : ''}`} onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div ref={ref} role="dialog" aria-modal="true" aria-label={typeof title === 'string' ? title : undefined} className={['rb-dialog', wide && 'rb-dialog--wide', className].filter(Boolean).join(' ')}>
        {critical && <div className="rb-dialog__critical-rule" aria-hidden />}
        {title && (
          <div className="rb-dialog__head">
            <h2 className="rb-dialog__title">{title}</h2>
          </div>
        )}
        <div className="rb-dialog__body">{children}</div>
        {footer && <div className="rb-dialog__foot">{footer}</div>}
      </div>
    </div>,
    document.body,
  );
}
