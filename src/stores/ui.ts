import { create } from 'zustand';

import type { Confirmation } from '@/bindings/Confirmation';
import type { ConfirmationRequest } from '@/bindings/ConfirmationRequest';
import type { Engine } from '@/bindings/Engine';
import type { Guarded } from '@/bindings/Guarded';
import type { ParsedUrl } from '@/bindings/ParsedUrl';
import { errorMessage } from '@/lib/errors';
import { uid } from '@/lib/id';

export type SettingsSection = 'general' | 'editor' | 'results' | 'security' | 'engines' | 'about';

export type View =
  | { name: 'home' }
  | { name: 'engines' }
  | { name: 'connection-form'; engine: Engine; connectionId?: string; prefill?: ParsedUrl }
  | { name: 'workspace' }
  | { name: 'history' }
  | { name: 'settings'; section: SettingsSection };

export type ToastTone = 'info' | 'positive' | 'critical';

interface Toast {
  id: string;
  tone: ToastTone;
  message: string;
}

interface AskOptions {
  title: string;
  body: string;
  confirmLabel: string;
  danger?: boolean;
}

interface UiState {
  view: View;
  /** Where Back returns to from History and Settings. */
  returnTo: View;
  toasts: Toast[];
  confirmation: { request: ConfirmationRequest; resolve: (c: Confirmation | null) => void } | null;
  question: (AskOptions & { resolve: (ok: boolean) => void }) | null;
  paletteOpen: boolean;

  navigate: (view: View) => void;
  back: () => void;
  toast: (message: string, tone?: ToastTone) => void;
  toastError: (error: unknown) => void;
  dismissToast: (id: string) => void;
  requestConfirmation: (request: ConfirmationRequest) => Promise<Confirmation | null>;
  ask: (options: AskOptions) => Promise<boolean>;
  setPaletteOpen: (open: boolean) => void;
}

const TOAST_MS = 5000;

export const useUi = create<UiState>((set, get) => ({
  view: { name: 'home' },
  returnTo: { name: 'home' },
  toasts: [],
  confirmation: null,
  question: null,
  paletteOpen: false,

  navigate: (view) => {
    const current = get().view;
    const overlay = view.name === 'history' || view.name === 'settings';
    const fromOverlay = current.name === 'history' || current.name === 'settings';
    set({ view, returnTo: overlay && !fromOverlay ? current : get().returnTo });
  },
  back: () => set({ view: get().returnTo }),

  toast: (message, tone = 'info') => {
    const id = uid('toast');
    set({ toasts: [...get().toasts, { id, tone, message }].slice(-4) });
    window.setTimeout(() => get().dismissToast(id), tone === 'critical' ? TOAST_MS * 2 : TOAST_MS);
  },
  toastError: (error) => get().toast(errorMessage(error), 'critical'),
  dismissToast: (id) => set({ toasts: get().toasts.filter((t) => t.id !== id) }),

  requestConfirmation: (request) =>
    new Promise((resolve) =>
      set({
        confirmation: {
          request,
          resolve: (c) => {
            set({ confirmation: null });
            resolve(c);
          },
        },
      }),
    ),

  ask: (options) =>
    new Promise((resolve) =>
      set({
        question: {
          ...options,
          resolve: (ok) => {
            set({ question: null });
            resolve(ok);
          },
        },
      }),
    ),

  setPaletteOpen: (paletteOpen) => set({ paletteOpen }),
}));

/**
 * Runs a guarded command. If the backend asks for confirmation, shows the
 * dialog and retries with the user's answer. Resolves `null` when declined.
 */
export async function guarded<T>(
  run: (confirmation: Confirmation | null) => Promise<Guarded<T>>,
  hooks: { onAsk?: () => void; onConfirmed?: () => void } = {},
): Promise<T | null> {
  const first = await run(null);
  if (first.status === 'done') return first.value;
  hooks.onAsk?.();
  const confirmation = await useUi.getState().requestConfirmation(first.request);
  if (!confirmation) return null;
  hooks.onConfirmed?.();
  const second = await run(confirmation);
  return second.status === 'done' ? second.value : null;
}
