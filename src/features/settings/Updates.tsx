import { relaunch } from '@tauri-apps/plugin-process';
import { check, type Update } from '@tauri-apps/plugin-updater';
import { useState } from 'react';

import { Alert, Button, Card, Checkbox } from '@/components/ui';
import { formatBytes } from '@/lib/format';
import { useSettings } from '@/stores/settings';

type State =
  | { step: 'idle' }
  | { step: 'checking' }
  | { step: 'none'; checkedAt: number }
  | { step: 'available'; update: Update }
  | { step: 'downloading'; update: Update; done: number; total: number | null }
  | { step: 'ready'; update: Update }
  | { step: 'error'; message: string };

/** Looks for a signed update; asks before downloading; installs and offers a restart. */
export async function checkForUpdate(): Promise<Update | null> {
  return check({ timeout: 30_000 });
}

/**
 * Signed updates: the release feed is checked only when asked (or at launch,
 * if enabled). The download's minisign signature is verified against the
 * public key built into the app before anything is installed.
 */
export function UpdatesCard() {
  const { settings, update } = useSettings();
  const [state, setState] = useState<State>({ step: 'idle' });

  const run = async () => {
    setState({ step: 'checking' });
    try {
      const found = await checkForUpdate();
      setState(found ? { step: 'available', update: found } : { step: 'none', checkedAt: Date.now() });
    } catch (e) {
      setState({ step: 'error', message: String((e as Error)?.message ?? e) });
    }
  };

  const install = async (u: Update) => {
    let done = 0;
    let total: number | null = null;
    setState({ step: 'downloading', update: u, done, total });
    try {
      await u.downloadAndInstall((event) => {
        if (event.event === 'Started') total = event.data.contentLength ?? null;
        if (event.event === 'Progress') done += event.data.chunkLength;
        setState({ step: 'downloading', update: u, done, total });
      });
      setState({ step: 'ready', update: u });
    } catch (e) {
      setState({ step: 'error', message: `The update was not installed: ${String((e as Error)?.message ?? e)}` });
    }
  };

  return (
    <Card title="Updates" footer="Updates are signed. The signature is checked against the key built into DataQuery before installing; an unsigned or altered download is refused.">
      <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <Button busy={state.step === 'checking'} disabled={state.step === 'downloading'} onClick={() => void run()}>
            Check for updates
          </Button>
          <span className="ink-2" style={{ fontSize: 12 }}>
            Update channel: stable
          </span>
        </div>
        {state.step === 'none' && <Alert tone="positive">You have the latest version.</Alert>}
        {state.step === 'error' && <Alert tone="critical">{state.message}</Alert>}
        {(state.step === 'available' || state.step === 'downloading') && (
          <Alert tone="info" title={`${state.update.version} is available`}>
            {state.update.date && <div className="ink-2">Released {new Date(state.update.date).toLocaleDateString()}</div>}
            {state.update.body && <div style={{ whiteSpace: 'pre-wrap', marginTop: 6 }}>{state.update.body}</div>}
            <div style={{ marginTop: 8 }}>
              {state.step === 'available' ? (
                <Button variant="primary" onClick={() => void install(state.update)}>
                  Download and install
                </Button>
              ) : (
                <span className="tabular">
                  Downloading {formatBytes(state.done)}
                  {state.total ? ` of ${formatBytes(state.total)}` : ''}…
                </span>
              )}
            </div>
          </Alert>
        )}
        {state.step === 'ready' && (
          <Alert tone="positive" title={`${state.update.version} is installed`}>
            Signature verified. Restart to finish.
            <div style={{ marginTop: 8 }}>
              <Button variant="primary" onClick={() => void relaunch()}>
                Restart now
              </Button>
            </div>
          </Alert>
        )}
        <Checkbox
          label="Check for updates when DataQuery starts"
          help="Off by default: DataQuery makes no network requests of its own unless you ask."
          checked={settings.check_updates_on_launch}
          onChange={(v) => void update({ check_updates_on_launch: v })}
        />
      </div>
    </Card>
  );
}
