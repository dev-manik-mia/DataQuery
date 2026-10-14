import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import { createPortal } from 'react-dom';

export type MenuEntry =
  | { type?: 'item'; label: ReactNode; onSelect: () => void; hint?: string; danger?: boolean; disabled?: boolean }
  | { type: 'separator' }
  | { type: 'label'; label: ReactNode };

interface MenuProps {
  x: number;
  y: number;
  items: MenuEntry[];
  onClose: () => void;
  label?: string;
}

/** Context menu at a screen position. Arrow keys move, Enter selects, Escape closes. */
export function Menu({ x, y, items, onClose, label = 'Actions' }: MenuProps) {
  const ref = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState({ left: x, top: y });

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const { width, height } = el.getBoundingClientRect();
    setPosition({ left: Math.min(x, window.innerWidth - width - 8), top: Math.min(y, window.innerHeight - height - 8) });
    el.querySelector<HTMLElement>('[role="menuitem"]:not([disabled])')?.focus();
  }, [x, y]);

  useEffect(() => {
    const close = (e: Event) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        onClose();
      }
    };
    document.addEventListener('mousedown', close, true);
    document.addEventListener('keydown', onKey, true);
    window.addEventListener('blur', onClose);
    return () => {
      document.removeEventListener('mousedown', close, true);
      document.removeEventListener('keydown', onKey, true);
      window.removeEventListener('blur', onClose);
    };
  }, [onClose]);

  const move = (e: React.KeyboardEvent) => {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    e.preventDefault();
    const nodes = [...(ref.current?.querySelectorAll<HTMLElement>('[role="menuitem"]:not([disabled])') ?? [])];
    const i = nodes.indexOf(document.activeElement as HTMLElement);
    nodes[(i + (e.key === 'ArrowDown' ? 1 : -1) + nodes.length) % nodes.length]?.focus();
  };

  return createPortal(
    <div ref={ref} className="rb-menu" role="menu" aria-label={label} style={position} onKeyDown={move}>
      {items.map((item, i) => {
        if (item.type === 'separator') return <div key={i} className="rb-menu__sep" role="separator" />;
        if (item.type === 'label')
          return (
            <div key={i} className="rb-menu__label">
              {item.label}
            </div>
          );
        return (
          <button
            key={i}
            type="button"
            role="menuitem"
            disabled={item.disabled}
            className={`rb-menu__item${item.danger ? ' rb-menu__item--danger' : ''}`}
            onClick={() => {
              onClose();
              item.onSelect();
            }}
          >
            {item.label}
            {item.hint && <span className="rb-menu__hint">{item.hint}</span>}
          </button>
        );
      })}
    </div>,
    document.body,
  );
}

/** State helper for opening a context menu from a mouse event. */
export function useContextMenu<T = void>() {
  const [menu, setMenu] = useState<{ x: number; y: number; target: T } | null>(null);
  return {
    menu,
    open: (e: React.MouseEvent, target: T) => {
      e.preventDefault();
      setMenu({ x: e.clientX, y: e.clientY, target });
    },
    close: () => setMenu(null),
  };
}
