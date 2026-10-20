import { create } from 'zustand';

import type { FreeLimits } from '@/bindings/FreeLimits';
import type { LicenseStatus } from '@/bindings/LicenseStatus';
import { api } from '@/lib/ipc';

import { useUi } from './ui';

interface LicenseState {
  status: LicenseStatus | null;
  load: () => Promise<void>;
  activate: (key: string) => Promise<void>;
  deactivate: () => Promise<void>;
}

export const useLicense = create<LicenseState>((set) => ({
  status: null,
  load: async () => {
    try {
      set({ status: await api.licenseStatus() });
    } catch (e) {
      useUi.getState().toastError(e);
    }
  },
  activate: async (key) => set({ status: await api.licenseActivate(key) }),
  deactivate: async () => set({ status: await api.licenseDeactivate() }),
}));

/** Pro, or a build from source. Until the status loads, nothing is locked. */
export function isPro(status: LicenseStatus | null = useLicense.getState().status): boolean {
  return status?.plan !== 'free';
}

/**
 * True when one more `kind` fits in the Free plan given `count` already open;
 * otherwise says what the limit is and returns false.
 */
export function withinFreeLimit(kind: keyof FreeLimits, count: number): boolean {
  const limit = useLicense.getState().status?.limits?.[kind];
  if (limit === undefined || count < limit) return true;
  const noun = { tabs: 'tabs', windows: 'windows', filters: 'filter conditions' }[kind];
  useUi.getState().toast(`DataQuery Free allows ${limit} ${noun}. Pro removes the limit: one payment, lifetime updates (Settings → License).`);
  return false;
}
