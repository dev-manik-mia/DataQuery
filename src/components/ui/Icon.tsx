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
