import { useEffect, useState, type ReactNode } from 'react';

import type { AppInfo } from '@/bindings/AppInfo';
import type { Capabilities } from '@/bindings/Capabilities';
import type { KeychainStatus } from '@/bindings/KeychainStatus';
import type { Settings } from '@/bindings/Settings';
import type { Theme } from '@/bindings/Theme';
import { Card, Checkbox, Chip, EngineMark, PageHead, PlannedChip, Segmented, TextField } from '@/components/ui';
import { api } from '@/lib/ipc';
import { isMac, shortcutLabel } from '@/lib/platform';
import { useConnections } from '@/stores/connections';
import { useSettings } from '@/stores/settings';
import { useUi, type SettingsSection } from '@/stores/ui';

import styles from './Settings.module.css';

const SECTIONS: { id: SettingsSection; label: string }[] = [
  { id: 'general', label: 'General' },
  { id: 'editor', label: 'Editor' },
  { id: 'results', label: 'Results' },
  { id: 'security', label: 'Security and privacy' },
  { id: 'engines', label: 'Engines' },
  { id: 'about', label: 'About' },
];

export function SettingsPage({ section }: { section: SettingsSection }) {
  const navigate = useUi((s) => s.navigate);
  return (
    <div className={styles.page}>
      <nav className={styles.nav} aria-label="Settings sections">
        {SECTIONS.map((s) => (
          <button key={s.id} type="button" className={styles.navItem} aria-current={s.id === section ? 'page' : undefined} onClick={() => navigate({ name: 'settings', section: s.id })}>
            {s.label}
          </button>
        ))}
      </nav>
      <div className={styles.content}>
        <div className={`${styles.inner} ${section === 'engines' ? styles.wide : ''}`}>
          {section === 'general' && <General />}
          {section === 'editor' && <Editor />}
          {section === 'results' && <Results />}
          {section === 'security' && <Security />}
          {section === 'engines' && <Engines />}
          {section === 'about' && <About />}
        </div>
      </div>
    </div>
  );
}

function Row({ title, help, children }: { title: ReactNode; help?: ReactNode; children?: ReactNode }) {
  return (
    <div className={styles.row}>
      <div className={styles.rowText}>
        <span className={styles.rowTitle}>{title}</span>
        {help && <span className={styles.rowHelp}>{help}</span>}
      </div>
      {children}
    </div>
  );
}

/** Numeric setting that saves on blur with range clamping in the backend. */
function NumberSetting({ label, field, suffix, hint }: { label: string; field: keyof Pick<Settings, 'row_limit' | 'timeout_secs' | 'editor_font_size' | 'page_size'>; suffix: string; hint?: string }) {
  const { settings, update } = useSettings();
  const [value, setValue] = useState(String(settings[field]));
  useEffect(() => setValue(String(settings[field])), [settings, field]);
  const commit = () => {
    const n = Number(value);
    if (Number.isFinite(n) && n !== settings[field]) void update({ [field]: Math.round(n) });
    else setValue(String(settings[field]));
  };
  return (
    <TextField
      label={label}
      mono
      inputMode="numeric"
      suffix={suffix}
      hint={hint}
      value={value}
      onChange={(e) => setValue(e.target.value.replace(/\D/g, ''))}
      onBlur={commit}
      onKeyDown={(e) => e.key === 'Enter' && commit()}
    />
  );
}

function General() {
  const { settings, update } = useSettings();
  return (
    <>
      <PageHead eyebrow="Settings" title="General" />
      <Card title="Appearance" flush>
        <Row title="Theme" help="System follows your operating system's light or dark mode.">
          <Segmented<Theme>
            label="Theme"
            value={settings.theme}
            onChange={(theme) => void update({ theme })}
            options={[
              { value: 'dark', label: 'Dark' },
              { value: 'light', label: 'Light' },
              { value: 'system', label: 'System' },
            ]}
          />
        </Row>
        <Row title="Language" help="English only for now. Strings are kept in one place to make translation possible.">
          <PlannedChip priority="P1" />
        </Row>
      </Card>
    </>
  );
}

const SHORTCUTS: [string, string][] = [
  ['Run statement at cursor (or selection)', 'mod+enter'],
  ['Run whole script', 'shift+mod+enter'],
  ['New query tab', 'mod+t'],
  ['Close tab', 'mod+w'],
  ['Search objects and scripts', 'mod+k'],
  ['Save script or file', 'mod+s'],
  ['Find / replace in editor', 'mod+f'],
  ['Next / previous tab', isMac ? 'ctrl+tab' : 'ctrl+tab'],
];

function Editor() {
  return (
    <>
      <PageHead eyebrow="Settings" title="Editor" />
      <Card title="Font" flush>
        <div className={styles.fields}>
          <NumberSetting label="Editor font size" field="editor_font_size" suffix="px" hint="9–28 px. JetBrains Mono." />
        </div>
      </Card>
      <Card title="Keyboard" actions={<PlannedChip priority="P1" />} flush>
        <div className={styles.shortcuts}>
          {SHORTCUTS.map(([label, keys]) => (
            <div key={label} style={{ display: 'contents' }}>
              <span>{label}</span>
              <span className="dq-kbd">{shortcutLabel(keys)}</span>
            </div>
          ))}
        </div>
      </Card>
    </>
  );
}

function Results() {
  return (
    <>
      <PageHead eyebrow="Settings" title="Results" />
      <Card title="Bounds" flush>
        <div className={styles.fields}>
          <NumberSetting label="Default row limit" field="row_limit" suffix="rows" hint="Rows fetched before pausing. Fetch more on demand." />
          <NumberSetting label="Default statement timeout" field="timeout_secs" suffix="seconds" hint="0 disables the timeout." />
          <NumberSetting label="Table page size" field="page_size" suffix="rows" hint="Rows per page in the table data view." />
        </div>
      </Card>
    </>
  );
}

function Security() {
  const { settings, update } = useSettings();
  const [keychain, setKeychain] = useState<KeychainStatus | null>(null);
  useEffect(() => {
    api.keychainStatus().then(setKeychain).catch(() => setKeychain(null));
  }, []);
  return (
    <>
      <PageHead eyebrow="Settings" title="Security and privacy" />
      <Card title="Credentials" flush>
        <Row title="Secret storage" help={`Passwords are kept in ${keychain?.backend ?? 'the system keychain'}. Settings files hold host, port, database and username only.`}>
          {keychain ? (
            <Chip tone={keychain.available ? 'positive' : 'critical'} dot>
              {keychain.available ? 'Keychain available' : 'Keychain unavailable'}
            </Chip>
          ) : (
            <Chip tone="mute">Checking…</Chip>
          )}
        </Row>
        <Row title="Log redaction" help="Passwords, tokens and connection strings are removed from logs, history and error reports. Always on.">
          <Chip tone="mute">Always on</Chip>
        </Row>
      </Card>
      <Card title="Query safety" flush>
        <div className={styles.check}>
          <Checkbox
            label="Confirm DROP, TRUNCATE and DELETE/UPDATE without WHERE"
            help="On every connection. Production always asks and also requires typing the connection name."
            checked={settings.confirm_destructive}
            onChange={(v) => void update({ confirm_destructive: v })}
          />
        </div>
        <div className={styles.check}>
          <Checkbox label="Confirm every write on Production connections" help="Includes inline grid edits and imports." checked={settings.confirm_production_writes} onChange={(v) => void update({ confirm_production_writes: v })} />
        </div>
        <div className={styles.check}>
          <Checkbox label="Restore tabs on launch without running them" help="Always on. Restored tabs show their last SQL and no results." checked disabled onChange={() => undefined} />
        </div>
        <div className={styles.check}>
          <Checkbox label="Keep query history" help="Stores SQL text locally. Result rows are never stored." checked={settings.keep_history} onChange={(v) => void update({ keep_history: v })} />
        </div>
        <div className={styles.fields}>
          <NumberSetting label="Default statement timeout" field="timeout_secs" suffix="seconds" />
          <NumberSetting label="Default row limit" field="row_limit" suffix="rows" />
        </div>
      </Card>
      <Card title="Data sharing" flush>
        <Row title="Usage telemetry" help="Off. DataQuery sends nothing unless you turn this on. There is no account and no cloud service.">
          <Chip tone="mute">Off</Chip>
        </Row>
      </Card>
    </>
  );
}

const CAPABILITY_COLUMNS: { key: keyof Capabilities; label: string }[] = [
  { key: 'explorer', label: 'Explorer' },
  { key: 'query_editor', label: 'Query editor' },
  { key: 'grid_edit', label: 'Grid edit' },
  { key: 'import_export', label: 'Import/export' },
  { key: 'explain', label: 'Explain' },
  { key: 'monitoring', label: 'Monitoring' },
  { key: 'transactions', label: 'Transactions' },
  { key: 'schema_diff', label: 'Schema diff' },
];

/** Capability matrix (screen 15). Built-in engines report their runtime capabilities. */
function Engines() {
  const engines = useConnections((s) => s.engines);
  return (
    <>
      <PageHead eyebrow="Settings" title="Engines" />
      <Card title="Capabilities" flush footer="● shipped in this build · P0–P2 planned priority. Controls for unsupported capabilities are hidden for that engine.">
        <div className={styles.matrix}>
          <table>
            <thead>
              <tr>
                <th>Engine</th>
                <th>Language</th>
                {CAPABILITY_COLUMNS.map((c) => (
                  <th key={c.key}>{c.label}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {engines.map((e) => (
                <tr key={e.engine}>
                  <td>
                    <span className={styles.engineCell}>
                      <EngineMark engine={e} size={24} />
                      {e.name}
                    </span>
                  </td>
                  <td className="ink-2">{e.language}</td>
                  {CAPABILITY_COLUMNS.map((c) => (
                    <td key={c.key}>
                      {e.available && e.capabilities[c.key] ? (
                        <span className={styles.yes} aria-label="Supported">
                          ●
                        </span>
                      ) : (
                        <span className={e.available ? styles.no : styles.planned}>{e.available ? 'Planned' : e.priority === 'MVP' ? 'P0' : e.priority}</span>
                      )}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </Card>
    </>
  );
}

function About() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  useEffect(() => {
    api.appInfo().then(setInfo).catch(() => setInfo(null));
  }, []);
  const platform = info ? `${info.os === 'macos' ? 'macOS' : info.os === 'windows' ? 'Windows' : 'Linux'} · ${info.arch}` : '…';
  return (
    <>
      <PageHead eyebrow="Settings" title="About DataQuery" />
      <Card flush>
        <Row title="Version" help={info ? `DataQuery ${info.version}` : '…'} />
        <Row title="Platform" help={platform} />
        <Row title="Licence" help={`${info?.license ?? 'MIT OR Apache-2.0'}. Free, no account.`} />
        <Row title="Tested platforms" help="Developed on macOS (Apple silicon). Integration tests run against PostgreSQL 17, MySQL 8.4 and MariaDB 11.4. Windows and Linux builds compile from the same code but are not yet tested." />
      </Card>
      <Card title="Updates" flush>
        <Row title="Signed update check, download and install on restart" help="Needs a signing key and a release feed, which are not set up yet. Install new versions manually for now.">
          <PlannedChip priority="P0" />
        </Row>
      </Card>
      <Card title={`Release notes · ${info?.version ?? ''}`} flush>
        <ul className={styles.notes}>
          <li>PostgreSQL, MySQL and MariaDB adapters: explorer, SQL editor, bounded results, cancel, reconnect.</li>
          <li>Table data view with filter, sort, paging and safe inline edits applied in one transaction.</li>
          <li>Streamed CSV/JSON export, CSV import, query history and saved scripts.</li>
          <li>Passwords in the system keychain; production safeguards enforced by the backend.</li>
        </ul>
      </Card>
    </>
  );
}
