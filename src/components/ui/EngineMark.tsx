import type { EngineInfo } from '@/bindings/EngineInfo';
import { ENGINE_ICONS, type EngineIcon } from './engineIcons';

/** Engine logo on a brand-colored tile; falls back to the two-letter monogram on a hue-coded tile. */
export function EngineMark({
  engine,
  size = 28,
}: {
  engine: Pick<EngineInfo, 'engine' | 'mark' | 'hue' | 'name'>;
  size?: number;
}) {
  const icon: EngineIcon | undefined = ENGINE_ICONS[engine.engine];
  return (
    <span
      className="dq-engine"
      title={engine.name}
      aria-label={engine.name}
      role="img"
      style={{
        width: size,
        height: size,
        fontSize: size >= 32 ? 12 : 11,
        background: icon?.color ?? `oklch(0.82 0.1 ${engine.hue})`,
      }}
    >
      {icon ? (
        <svg viewBox={icon.viewBox} width={Math.round(size * 0.62)} height={Math.round(size * 0.62)} fill={icon.fg} aria-hidden="true">
          <path fillRule="evenodd" d={icon.d} />
        </svg>
      ) : (
        engine.mark
      )}
    </span>
  );
}
