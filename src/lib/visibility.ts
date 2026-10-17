import { useSyncExternalStore } from 'react';

function subscribe(onChange: () => void) {
  document.addEventListener('visibilitychange', onChange);
  return () => document.removeEventListener('visibilitychange', onChange);
}

/** False while the window is minimized or hidden, so polling can pause. */
export function usePageVisible(): boolean {
  return useSyncExternalStore(subscribe, () => document.visibilityState !== 'hidden');
}
