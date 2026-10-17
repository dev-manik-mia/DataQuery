import { useShallow } from 'zustand/react/shallow';
import { Alert, Button, Card, Checkbox } from '@/components/ui';
import { formatBytes } from '@/lib/format';
import { useSettings } from '@/stores/settings';
import { useUpdates } from '@/stores/updates';

/**
 * Signed updates: the release feed is checked on request, or at launch and
 * every few hours when automatic checks are on. With automatic install the
 * update downloads in the background and applies on the next restart. The
 * download's minisign signature is verified against the public key built into
 * the app before anything is installed.
 */
export function UpdatesCard() {
  const { settings, update } = useSettings(useShallow((s) => ({ settings: s.settings, update: s.update })));
  const { state, check, install, restart } = useUpdates();
  const busy = state.step === 'checking' || state.step === 'downloading';

  return (
    <Card title="Updates" footer="Updates are signed. The signature is checked against the key built into DataQuery before installing; an unsigned or altered download is refused.">
      <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <Button busy={state.step === 'checking'} disabled={busy || state.step === 'ready'} onClick={() => void check()}>
            Check for updates
          </Button>
          <span className="ink-2" style={{ fontSize: 12 }}>
            Update channel: stable
          </span>
        </div>
        {state.step === 'none' && <Alert tone="positive">You have the latest version.</Alert>}
        {state.step === 'error' && <Alert tone="critical">{state.message}</Alert>}
        {(state.step === 'available' || state.step === 'downloading') && (
          <Alert tone="info" title={`Update available: ${state.update.version}`}>
            {state.update.date && <div className="ink-2">Released {new Date(state.update.date).toLocaleDateString()}</div>}
            {state.update.body && <div style={{ whiteSpace: 'pre-wrap', marginTop: 6 }}>{state.update.body}</div>}
            <div style={{ marginTop: 8 }}>
              {state.step === 'available' ? (
                <Button variant="primary" onClick={() => void install()}>
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
          <Alert tone="positive" title={`${state.update.version} is ready`}>
            Signature verified. Restart to finish.
            <div style={{ marginTop: 8 }}>
              <Button variant="primary" onClick={() => void restart()}>
                Restart now
              </Button>
            </div>
          </Alert>
        )}
        <Checkbox
          label="Check for updates automatically"
          help="Looks for a new release at launch and every 6 hours, and tells you when one is available."
          checked={settings.check_updates_on_launch}
          onChange={(v) => void update(v ? { check_updates_on_launch: true } : { check_updates_on_launch: false, auto_install_updates: false })}
        />
        <Checkbox
          label="Download and install updates automatically"
          help="Updates install in the background and apply the next time DataQuery restarts. Nothing restarts without you."
          checked={settings.auto_install_updates}
          onChange={(v) => void update(v ? { auto_install_updates: true, check_updates_on_launch: true } : { auto_install_updates: false })}
        />
      </div>
    </Card>
  );
}
