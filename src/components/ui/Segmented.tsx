import { useRef, type KeyboardEvent, type ReactNode } from 'react';

export interface SegmentOption<T extends string> {
  value: T;
  label: ReactNode;
  count?: ReactNode;
  disabled?: boolean;
  /** Overrides the selected style, e.g. environment colours. */
  selectedClassName?: string;
}

interface SegmentedProps<T extends string> {
  label: string;
  value: T;
  options: SegmentOption<T>[];
  onChange: (value: T) => void;
  size?: 'sm' | 'md';
}

/** A radiogroup: arrow keys move, only the selected option is tabbable. */
export function Segmented<T extends string>({ label, value, options, onChange, size = 'sm' }: SegmentedProps<T>) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  const enabled = options.filter((o) => !o.disabled);

  const onKeyDown = (e: KeyboardEvent) => {
    const step = e.key === 'ArrowRight' || e.key === 'ArrowDown' ? 1 : e.key === 'ArrowLeft' || e.key === 'ArrowUp' ? -1 : 0;
    if (!step) return;
    e.preventDefault();
    const i = enabled.findIndex((o) => o.value === value);
    const next = enabled[(i + step + enabled.length) % enabled.length];
    if (next) {
      onChange(next.value);
      refs.current[options.indexOf(next)]?.focus();
    }
  };

  return (
    <div role="radiogroup" aria-label={label} className={`dq-seg${size === 'md' ? ' dq-seg--md' : ''}`} onKeyDown={onKeyDown}>
      {options.map((o, i) => {
        const selected = o.value === value;
        return (
          <button
            key={o.value}
            ref={(el) => {
              refs.current[i] = el;
            }}
            type="button"
            role="radio"
            aria-checked={selected}
            tabIndex={selected ? 0 : -1}
            disabled={o.disabled}
            className={['dq-seg__opt', selected && o.selectedClassName].filter(Boolean).join(' ')}
            onClick={() => onChange(o.value)}
          >
            {o.label}
            {o.count !== undefined && <span className="dq-seg__count">{o.count}</span>}
          </button>
        );
      })}
    </div>
  );
}
