import type { ReactNode } from 'react';

import type { Environment } from '@/bindings/Environment';
import type { Priority } from '@/bindings/Priority';

export type ChipTone = 'positive' | 'caution' | 'critical' | 'accent' | 'mute' | 'faint';

export function Chip({ tone, dot = false, small = false, title, children }: { tone: ChipTone; dot?: boolean; small?: boolean; title?: string; children: ReactNode }) {
  return (
    <span className={`dq-chip dq-chip--${tone}${small ? ' dq-chip--sm' : ''}`} title={title}>
      {dot && <span className="dq-chip__dot" aria-hidden />}
      {children}
    </span>
  );
}

export const environmentTone: Record<Environment, ChipTone> = {
  Local: 'mute',
  Development: 'positive',
  Staging: 'caution',
  Production: 'critical',
};

/** Environment label: word plus colour, never colour alone. */
export function EnvironmentChip({ environment, small }: { environment: Environment; small?: boolean }) {
  return (
    <Chip tone={environmentTone[environment]} small={small}>
      {environment}
    </Chip>
  );
}

const priorityTone: Record<Priority, ChipTone> = { MVP: 'positive', P0: 'accent', P1: 'mute', P2: 'faint' };

export function PriorityChip({ priority, available, beta }: { priority: Priority; available?: boolean; beta?: boolean }) {
  if (available && beta)
    return (
      <Chip tone="caution" small title="Tested against an emulator only, not yet against the real service">
        Beta
      </Chip>
    );
  if (available) return <Chip tone="positive" small>Stable</Chip>;
  return (
    <Chip tone={priorityTone[priority]} small>
      {priority}
    </Chip>
  );
}

/** "Planned · P1" marker for features that are visible but not built yet. */
export function PlannedChip({ priority }: { priority: string }) {
  return <Chip tone="mute">Planned · {priority}</Chip>;
}
