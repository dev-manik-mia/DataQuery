import { listen } from '@tauri-apps/api/event';
import { useEffect, useState } from 'react';

import type { TransferProgress } from '@/bindings/TransferProgress';

/** Live rows/bytes of a running export or import job, from `transfer-progress` events. */
export function useTransferProgress(jobId: string | null): TransferProgress | null {
  const [progress, setProgress] = useState<TransferProgress | null>(null);
  useEffect(() => {
    setProgress(null);
    if (!jobId) return;
    const unlisten = listen<TransferProgress>('transfer-progress', (e) => {
      if (e.payload.job_id === jobId) setProgress(e.payload);
    });
    return () => void unlisten.then((fn) => fn());
  }, [jobId]);
  return progress;
}
