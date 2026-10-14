import { useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import { Alert, Button } from '@/components/ui';
import { api } from '@/lib/ipc';
import { useUi } from '@/stores/ui';

const UNKNOWN_HOST_KEY = 'SSH_HOST_KEY_UNKNOWN';

/** Parses `Host h:22 · ssh-ed25519 · SHA256:…` from an unknown-host-key error. */
export function unknownHostKey(error: DbError): { host: string; port: number; algorithm: string; fingerprint: string } | null {
  if (error.code !== UNKNOWN_HOST_KEY || !error.detail) return null;
  const m = /^Host (.+):(\d+) · (\S+) · (SHA256:\S+)$/.exec(error.detail);
  return m ? { host: m[1]!, port: Number(m[2]), algorithm: m[3]!, fingerprint: m[4]! } : null;
}

/** Shown under an SSH error for an unknown host: trust the key after comparing its fingerprint. */
export function TrustHostKey({ error, onTrusted }: { error: DbError; onTrusted: () => void }) {
  const key = unknownHostKey(error);
  const [busy, setBusy] = useState(false);
  if (!key) return null;
  const trust = async () => {
    setBusy(true);
    try {
      await api.trustHostKey(key.host, key.port, key.fingerprint);
      useUi.getState().toast(`Trusted the host key of ${key.host}.`, 'positive');
      onTrusted();
    } catch (e) {
      useUi.getState().toastError(e);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Alert tone="caution" title={`Trust ${key.host}:${key.port}?`}>
      <div>
        {key.algorithm} key fingerprint <span className="mono">{key.fingerprint}</span>
      </div>
      <div>Only trust it if it matches the fingerprint published by the server's administrator. It is saved in DataQuery's known_hosts.</div>
      <div style={{ marginTop: 8 }}>
        <Button size="sm" variant="caution" busy={busy} onClick={() => void trust()}>
          Trust this key and retry
        </Button>
      </div>
    </Alert>
  );
}
