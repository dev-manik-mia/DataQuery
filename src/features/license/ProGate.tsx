import type { ReactNode } from 'react';

import { Button, Card, PageHead } from '@/components/ui';
import { api } from '@/lib/ipc';
import { isPro, useLicense } from '@/stores/license';
import { useUi } from '@/stores/ui';

/** Shows `children` on Pro (and in builds from source); otherwise what the feature is and how to get it. */
export function ProGate({ feature, summary, children }: { feature: string; summary: string; children: ReactNode }) {
  const status = useLicense((s) => s.status);
  const navigate = useUi((s) => s.navigate);
  if (isPro(status)) return children;
  return (
    <div style={{ padding: 24, maxWidth: 640 }}>
      <PageHead eyebrow="DataQuery Pro" title={feature} />
      <Card
        footer={
          <div style={{ display: 'flex', gap: 8, justifyContent: 'flex-end' }}>
            <Button size="sm" onClick={() => navigate({ name: 'settings', section: 'license' })}>
              Enter licence key
            </Button>
            <Button size="sm" variant="primary" onClick={() => void api.openPricing().catch(useUi.getState().toastError)}>
              Get Pro — $59 once
            </Button>
          </div>
        }
      >
        <p style={{ margin: 0 }}>{summary}</p>
        <p className="muted" style={{ marginBottom: 0 }}>
          Pro is one payment with lifetime updates, for up to 3 of your devices. It also removes the Free limits of 4 tabs, 4 windows and 4 filter conditions.
        </p>
      </Card>
    </div>
  );
}
