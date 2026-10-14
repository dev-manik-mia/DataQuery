import type { ReactNode } from 'react';

import { Icon } from './Icon';

interface CheckboxProps {
  checked: boolean;
  onChange: (checked: boolean) => void;
  label: ReactNode;
  help?: ReactNode;
  disabled?: boolean;
}

export function Checkbox({ checked, onChange, label, help, disabled }: CheckboxProps) {
  return (
    <label className="rb-check">
      <input type="checkbox" checked={checked} disabled={disabled} onChange={(e) => onChange(e.target.checked)} />
      <span className="rb-check__box" aria-hidden>
        <Icon name="check" size={12} />
      </span>
      <span className="rb-check__text">
        <span className="rb-check__label">{label}</span>
        {help && <span className="rb-check__help">{help}</span>}
      </span>
    </label>
  );
}
