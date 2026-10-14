import { useId, type ComponentPropsWithRef, type ReactNode, type SelectHTMLAttributes, type TextareaHTMLAttributes } from 'react';

interface FieldShellProps {
  label: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  className?: string;
  children: (id: string, describedBy: string | undefined) => ReactNode;
}

/** Label, control, hint and error wired together for accessibility. */
export function FieldShell({ label, hint, error, className, children }: FieldShellProps) {
  const id = useId();
  const describedBy = error ? `${id}-error` : hint ? `${id}-hint` : undefined;
  return (
    <div className={['dq-field', className].filter(Boolean).join(' ')}>
      <label className="dq-field__label" htmlFor={id}>
        {label}
      </label>
      {children(id, describedBy)}
      {error ? (
        <span id={`${id}-error`} className="dq-field__error">
          {error}
        </span>
      ) : (
        hint && (
          <span id={`${id}-hint`} className="dq-field__hint">
            {hint}
          </span>
        )
      )}
    </div>
  );
}

type Common = { label: ReactNode; hint?: ReactNode; error?: ReactNode; mono?: boolean; className?: string };

function inputClass(mono?: boolean, small?: boolean) {
  return ['dq-input', mono && 'dq-input--mono', small && 'dq-input--sm'].filter(Boolean).join(' ');
}

export function TextField({
  label,
  hint,
  error,
  mono,
  className,
  suffix,
  ...rest
}: Common & ComponentPropsWithRef<'input'> & { suffix?: string }) {
  return (
    <FieldShell label={label} hint={hint} error={error} className={className}>
      {(id, describedBy) => {
        const input = (
          <input
            id={id}
            className={inputClass(mono)}
            aria-invalid={error ? true : undefined}
            aria-describedby={describedBy}
            spellCheck={false}
            autoCorrect="off"
            autoCapitalize="off"
            {...rest}
          />
        );
        return suffix ? (
          <span className="dq-input-wrap">
            {input}
            <span className="dq-input-wrap__suffix">{suffix}</span>
          </span>
        ) : (
          input
        );
      }}
    </FieldShell>
  );
}

export function SelectField({
  label,
  hint,
  error,
  mono,
  className,
  options,
  ...rest
}: Common & SelectHTMLAttributes<HTMLSelectElement> & { options: { value: string; label: string }[] }) {
  return (
    <FieldShell label={label} hint={hint} error={error} className={className}>
      {(id, describedBy) => (
        <select id={id} className={inputClass(mono)} aria-describedby={describedBy} {...rest}>
          {options.map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </select>
      )}
    </FieldShell>
  );
}

export function TextAreaField({ label, hint, error, mono, className, ...rest }: Common & TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return (
    <FieldShell label={label} hint={hint} error={error} className={className}>
      {(id, describedBy) => <textarea id={id} className={inputClass(mono)} aria-describedby={describedBy} spellCheck={false} {...rest} />}
    </FieldShell>
  );
}

/** Bare input for toolbars and filters (label is required for screen readers). */
export function Input({
  label,
  mono,
  small = true,
  className,
  ...rest
}: { label: string; mono?: boolean; small?: boolean } & ComponentPropsWithRef<'input'>) {
  return (
    <input
      aria-label={label}
      className={[inputClass(mono, small), className].filter(Boolean).join(' ')}
      spellCheck={false}
      autoCorrect="off"
      autoCapitalize="off"
      {...rest}
    />
  );
}
