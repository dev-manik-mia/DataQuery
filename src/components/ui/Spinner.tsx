export function Spinner({ current = false, label }: { current?: boolean; label?: string }) {
  return (
    <span
      className={current ? 'rb-spinner rb-spinner--current' : 'rb-spinner'}
      role={label ? 'status' : undefined}
      aria-label={label}
    />
  );
}

export function Skeleton({ width = '100%', height = 10 }: { width?: number | string; height?: number }) {
  return <span className="rb-skeleton" style={{ width, height }} aria-hidden />;
}
