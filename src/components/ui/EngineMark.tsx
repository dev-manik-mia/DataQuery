import type { EngineInfo } from '@/bindings/EngineInfo';

/** Two-letter engine monogram on a hue-coded tile. */
export function EngineMark({ engine, size = 28 }: { engine: Pick<EngineInfo, 'mark' | 'hue' | 'name'>; size?: number }) {
  return (
    <span
      className="rb-engine"
      title={engine.name}
      aria-label={engine.name}
      role="img"
      style={{ width: size, height: size, fontSize: size >= 32 ? 12 : 11, background: `oklch(0.82 0.1 ${engine.hue})` }}
    >
      {engine.mark}
    </span>
  );
}
