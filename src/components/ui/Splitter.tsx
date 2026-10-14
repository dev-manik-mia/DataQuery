import { useRef, useState } from 'react';

interface SplitterProps {
  axis: 'x' | 'y';
  /** Current size of the panel this splitter resizes. */
  size: number;
  min: number;
  max: number;
  onResize: (size: number) => void;
  /** -1 when dragging towards the start grows the panel (panel after the splitter). */
  direction?: 1 | -1;
  label: string;
}

/** Drag or use arrow keys to resize the adjacent panel. */
export function Splitter({ axis, size, min, max, onResize, direction = 1, label }: SplitterProps) {
  const [dragging, setDragging] = useState(false);
  const start = useRef({ pointer: 0, size: 0 });
  const clamp = (v: number) => Math.round(Math.min(max, Math.max(min, v)));

  const onPointerDown = (e: React.PointerEvent) => {
    e.preventDefault();
    (e.target as HTMLElement).setPointerCapture(e.pointerId);
    start.current = { pointer: axis === 'x' ? e.clientX : e.clientY, size };
    setDragging(true);
  };

  const onPointerMove = (e: React.PointerEvent) => {
    if (!dragging) return;
    const delta = (axis === 'x' ? e.clientX : e.clientY) - start.current.pointer;
    onResize(clamp(start.current.size + delta * direction));
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    const grow = axis === 'x' ? 'ArrowRight' : 'ArrowDown';
    const shrink = axis === 'x' ? 'ArrowLeft' : 'ArrowUp';
    if (e.key !== grow && e.key !== shrink) return;
    e.preventDefault();
    onResize(clamp(size + (e.key === grow ? 16 : -16) * direction));
  };

  return (
    <div
      role="separator"
      aria-orientation={axis === 'x' ? 'vertical' : 'horizontal'}
      aria-label={label}
      aria-valuenow={size}
      aria-valuemin={min}
      aria-valuemax={max}
      tabIndex={0}
      className={`rb-splitter rb-splitter--${axis}`}
      data-dragging={dragging}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={() => setDragging(false)}
      onKeyDown={onKeyDown}
    />
  );
}
