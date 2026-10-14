import type { SVGProps } from 'react';

/** 16px stroke icons. Decorative unless a `title` is given. */
const paths = {
  chevronRight: 'M6 4l4 4-4 4',
  chevronDown: 'M4 6l4 4 4-4',
  refresh: 'M13 8a5 5 0 1 1-1.5-3.5M13 3v3h-3',
  plus: 'M8 3v10M3 8h10',
  minus: 'M3 8h10',
  close: 'M4 4l8 8M12 4l-8 8',
  search: 'M7 12a5 5 0 1 0 0-10 5 5 0 0 0 0 10zM14 14l-3.5-3.5',
  star: 'M8 2l1.8 3.9 4.2.5-3.1 2.9.8 4.2L8 11.4l-3.7 2.1.8-4.2L2 6.4l4.2-.5z',
  copy: 'M5 5h8v8H5zM3 11V3h8',
  download: 'M8 2v8M4.5 6.5L8 10l3.5-3.5M3 13h10',
  upload: 'M8 11V3M4.5 6.5L8 3l3.5 3.5M3 13h10',
  key: 'M10 7a3 3 0 1 0-3 3l-4 4h2l1-1v-1h1l1-1M10 7h.01',
  play: 'M5 3l8 5-8 5z',
  stop: 'M4 4h8v8H4z',
  more: 'M3.5 8h.01M8 8h.01M12.5 8h.01',
  external: 'M9 3h4v4M13 3L7 9M11 9v4H3V5h4',
  folder: 'M2 4h4l1.5 1.5H14V13H2z',
  check: 'M3 8.5l3 3 7-7',
  warning: 'M8 2l6.5 11.5h-13zM8 6.5v3M8 11.5h.01',
  arrowUp: 'M8 13V3M4 7l4-4 4 4',
  arrowDown: 'M8 3v10M4 9l4 4 4-4',
  file: 'M4 2h5l3 3v9H4zM9 2v3h3',
  save: 'M3 3h8l2 2v8H3zM5 3v3h5V3M5 13V9h6v4',
  eye: 'M1.5 8S4 3.5 8 3.5 14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8zM8 10a2 2 0 1 0 0-4 2 2 0 0 0 0 4z',
  eyeOff: 'M2 2l12 12M6.6 6.6a2 2 0 0 0 2.8 2.8M4.2 4.7C2.5 5.9 1.5 8 1.5 8S4 12.5 8 12.5c1.3 0 2.4-.4 3.3-1M7 3.6c.3 0 .7-.1 1-.1 4 0 6.5 4.5 6.5 4.5s-.6 1.1-1.7 2.2',
  pin: 'M6 2h4l-.5 4 2.5 2v1H4V8l2.5-2zM8 9v5',
  link: 'M7 9l2-2M6 5l1-1a2.8 2.8 0 0 1 4 4l-1 1M10 11l-1 1a2.8 2.8 0 0 1-4-4l1-1',
  lock: 'M4 7h8v6H4zM5.5 7V5a2.5 2.5 0 0 1 5 0v2',
  filter: 'M2 3h12l-4.5 5.5V13l-3-1.5v-3z',
  columns: 'M2 3h12v10H2zM6 3v10M10 3v10',
  undo: 'M5 3L2 6l3 3M2 6h7a4 4 0 0 1 0 8H7',
  redo: 'M11 3l3 3-3 3M14 6H7a4 4 0 0 0 0 8h2',
  terminal: 'M2 3h12v10H2zM4.5 6.5L6.5 8l-2 1.5M8 10h3',
  diagram: 'M2 2h5v4H2zM9 10h5v4H9zM4.5 6v4h4.5',
  activity: 'M1.5 8h3l2-5 3 10 2-5h3',
  sparkle: 'M8 2l1.3 3.7L13 7l-3.7 1.3L8 12l-1.3-3.7L3 7l3.7-1.3zM13 11.5l.5 1.5 1.5.5-1.5.5-.5 1.5-.5-1.5L11 13.5l1.5-.5z',
  split: 'M2 3h12v10H2zM8 3v10',
  window: 'M2 3h12v10H2zM2 6h12',
  bookmark: 'M4 2h8v12l-4-3-4 3z',
  tag: 'M2 2h6l6 6-6 6-6-6zM5 5h.01',
  command: 'M6 6V4.5A1.5 1.5 0 1 0 4.5 6H6zm0 0h4m-4 0v4m4-4V4.5A1.5 1.5 0 1 1 11.5 6H10zm0 0v4m0 0h1.5a1.5 1.5 0 1 1-1.5 1.5V10zm0 0H6m0 0v1.5A1.5 1.5 0 1 1 4.5 10H6z',
  history: 'M2.5 8a5.5 5.5 0 1 0 1.6-3.9M2 2.5V5h2.5M8 5v3l2 1.5',
  settings: 'M8 10a2 2 0 1 0 0-4 2 2 0 0 0 0 4zM8 1.5v2M8 12.5v2M1.5 8h2M12.5 8h2M3.4 3.4l1.4 1.4M11.2 11.2l1.4 1.4M3.4 12.6l1.4-1.4M11.2 4.8l1.4-1.4',
} as const;

export type IconName = keyof typeof paths;

interface IconProps extends Omit<SVGProps<SVGSVGElement>, 'name'> {
  name: IconName;
  size?: number;
  title?: string;
  filled?: boolean;
}

export function Icon({ name, size = 14, title, filled = false, ...rest }: IconProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill={filled ? 'currentColor' : 'none'}
      stroke="currentColor"
      strokeWidth={1.5}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden={title ? undefined : true}
      role={title ? 'img' : undefined}
      {...rest}
    >
      {title && <title>{title}</title>}
      <path d={paths[name]} />
    </svg>
  );
}
