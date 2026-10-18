import { useEffect, useState, type ReactNode } from 'react';
import { useShallow } from 'zustand/react/shallow';

import type { AppInfo } from '@/bindings/AppInfo';
import type { Capabilities } from '@/bindings/Capabilities';
import type { Engine } from '@/bindings/Engine';
import type { KeychainStatus } from '@/bindings/KeychainStatus';
import type { Settings } from '@/bindings/Settings';
import type { Theme } from '@/bindings/Theme';
import type { AiProvider } from '@/bindings/AiProvider';
import type { ProviderInfo } from '@/bindings/ProviderInfo';
import { Button, Card, Checkbox, Chip, EngineMark, PageHead, Segmented, SelectField, TextField } from '@/components/ui';
import { openNewWindow } from '@/features/commands/commands';
import { languages, t } from '@/i18n';
import { api } from '@/lib/ipc';
import { useConnections } from '@/stores/connections';
import { useSettings } from '@/stores/settings';
import { useUi, type SettingsSection } from '@/stores/ui';
import { useUpdates } from '@/stores/updates';

import { KeyboardSettings } from './KeyboardSettings';
import styles from './Settings.module.css';
import { UpdatesCard } from './Updates';

const SECTIONS: SettingsSection[] = ['general', 'editor', 'keyboard', 'results', 'history', 'security', 'ai', 'engines', 'about'];

export function SettingsPage({ section }: { section: SettingsSection }) {
  const navigate = useUi((s) => s.navigate);
  const updateStep = useUpdates((s) => s.state.step);
  const updateBadge = updateStep === 'available' || updateStep === 'downloading' ? 'Update' : updateStep === 'ready' ? 'Restart' : null;
  return (
    <div className={styles.page}>
      <nav className={styles.nav} aria-label="Settings sections">
        {SECTIONS.map((id) => (
          <button key={id} type="button" className={styles.navItem} aria-current={id === section ? 'page' : undefined} onClick={() => navigate({ name: 'settings', section: id })}>
            {t(`settings.${id}`)}
            {id === 'about' && updateBadge && <span className={styles.navBadge}>{updateBadge}</span>}
          </button>
        ))}
      </nav>
      <div className={styles.content}>
        <div className={`${styles.inner} ${section === 'engines' ? styles.wide : ''}`}>
          {section === 'general' && <General />}
          {section === 'editor' && <Editor />}
          {section === 'keyboard' && <KeyboardSettings />}
          {section === 'results' && <Results />}
          {section === 'history' && <History />}
          {section === 'security' && <Security />}
          {section === 'ai' && <AiSettings />}
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
type NumericField = keyof Pick<Settings, 'row_limit' | 'timeout_secs' | 'editor_font_size' | 'page_size' | 'editor_line_height' | 'history_max_entries' | 'history_max_days'>;

function NumberSetting({ label, field, suffix, hint }: { label: string; field: NumericField; suffix: string; hint?: string }) {
  const { settings, update } = useSettings(useShallow((s) => ({ settings: s.settings, update: s.update })));
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
  const { settings, update } = useSettings(useShallow((s) => ({ settings: s.settings, update: s.update })));
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
        <Row title={t('settings.language')} help={t('settings.languageHelp')}>
          <select className="dq-input dq-input--sm" aria-label={t('settings.language')} value={settings.language} onChange={(e) => void update({ language: e.target.value })}>
            {languages.map((l) => (
              <option key={l} value={l}>
                {new Intl.DisplayNames([l], { type: 'language' }).of(l) ?? l}
              </option>
            ))}
          </select>
        </Row>
      </Card>
      <Card title="Windows" flush>
        <Row title="New window" help="Opens another window with its own workspace. Saved connections and settings are shared.">
          <Button size="sm" onClick={openNewWindow}>
            {t('action.newWindow')}
          </Button>
        </Row>
      </Card>
    </>
  );
}

function Editor() {
  const { settings, update } = useSettings(useShallow((s) => ({ settings: s.settings, update: s.update })));
  const [family, setFamily] = useState(settings.editor_font_family);
  useEffect(() => setFamily(settings.editor_font_family), [settings.editor_font_family]);
  return (
    <>
      <PageHead eyebrow="Settings" title="Editor" />
      <Card title="Font" flush>
        <div className={styles.fields}>
          <TextField
            label="Font family"
            hint="Any installed monospace font; JetBrains Mono ships with DataQuery."
            value={family}
            onChange={(e) => setFamily(e.target.value)}
            onBlur={() => family.trim() && family !== settings.editor_font_family && void update({ editor_font_family: family.trim() })}
          />
          <NumberSetting label="Editor font size" field="editor_font_size" suffix="px" hint="9–28 px." />
          <NumberSetting label="Line height" field="editor_line_height" suffix="%" hint="100–250% of the font size." />
        </div>
        <div className={styles.check}>
          <Checkbox label="Font ligatures" help="Combine characters such as -> and != when the font supports it." checked={settings.editor_ligatures} onChange={(v) => void update({ editor_ligatures: v })} />
        </div>
      </Card>
      <Card title="Autocomplete" flush>
        <Row title="Schema-aware completion" help="Tables, columns (also through aliases), keywords and your snippets. The catalog is read once per connection; reconnect to refresh it." />
        <Row title="Snippets" help="Manage them under History → Snippets.">
          <Button size="sm" onClick={() => useUi.getState().navigate({ name: 'history' })}>
            Open snippets
          </Button>
        </Row>
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

function History() {
  const { settings, update } = useSettings(useShallow((s) => ({ settings: s.settings, update: s.update })));
  const apply = () => void api.applyHistoryRetention().catch(useUi.getState().toastError);
  return (
    <>
      <PageHead eyebrow="Settings" title="History" />
      <Card title="Query history" flush>
        <div className={styles.check}>
          <Checkbox label="Keep query history" help="Stores SQL text locally. Result rows are never stored." checked={settings.keep_history} onChange={(v) => void update({ keep_history: v })} />
        </div>
        <div className={styles.check}>
          <Checkbox
            label="Redact string literals"
            help="Recorded SQL keeps its structure but shows '…' instead of quoted values. Passwords in SQL are always redacted."
            checked={settings.history_redact_literals}
            onChange={(v) => void update({ history_redact_literals: v })}
          />
        </div>
        <div className={styles.fields} onBlur={apply}>
          <NumberSetting label="Keep at most" field="history_max_entries" suffix="entries" hint="0 = no limit." />
          <NumberSetting label="Delete entries older than" field="history_max_days" suffix="days" hint="0 = keep." />
        </div>
      </Card>
      <Card title="Saved scripts" flush>
        <div className={styles.check}>
          <Checkbox
            label="Autosave scripts"
            help="Edits in a tab opened from a saved script are saved back to it after a short pause."
            checked={settings.autosave_scripts}
            onChange={(v) => void update({ autosave_scripts: v })}
          />
        </div>
      </Card>
    </>
  );
}

function Security() {
  const { settings, update } = useSettings(useShallow((s) => ({ settings: s.settings, update: s.update })));
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
      <Card title="Sensitive columns" flush>
        <div className={styles.check}>
          <Checkbox
            label="Mask sensitive columns"
            help="Values of columns whose names contain one of the patterns show as •••••• in grids and copies until revealed from the column menu."
            checked={settings.mask_columns}
            onChange={(v) => void update({ mask_columns: v })}
          />
        </div>
        <div className={styles.fields}>
          <MaskPatterns />
        </div>
      </Card>
      <Card title="Connections" flush>
        <Row title="Insecure-connection warnings" help="Connections with TLS off, optional or unverified to a host that is not this machine show a warning in the form and a “Not secure” chip in the title bar. Always on.">
          <Chip tone="mute">Always on</Chip>
        </Row>
        <Row title="Trusted SSH host keys" help="Keys you trust in DataQuery are stored in its own known_hosts file; ~/.ssh/known_hosts is read too. Changed keys are always refused." />
      </Card>
      <Card title="Data sharing" flush>
        <Row title="Usage telemetry" help="Off. DataQuery sends nothing unless you turn this on. There is no account and no cloud service.">
          <Chip tone="mute">Off</Chip>
        </Row>
        <Row title="AI assistant" help={settings.ai_enabled ? 'On. Each request shows what it sends; credentials, rows and results are never sent.' : 'Off. Nothing is sent to any model.'}>
          <Chip tone={settings.ai_enabled ? 'caution' : 'mute'}>{settings.ai_enabled ? 'On' : 'Off'}</Chip>
        </Row>
      </Card>
    </>
  );
}

function MaskPatterns() {
  const { settings, update } = useSettings(useShallow((s) => ({ settings: s.settings, update: s.update })));
  const [value, setValue] = useState(settings.mask_patterns.join(', '));
  useEffect(() => setValue(settings.mask_patterns.join(', ')), [settings.mask_patterns]);
  return (
    <TextField
      label="Patterns"
      mono
      hint="Comma-separated parts of column names, case-insensitive."
      value={value}
      onChange={(e) => setValue(e.target.value)}
      onBlur={() => void update({ mask_patterns: value.split(',').map((p) => p.trim()).filter(Boolean) })}
    />
  );
}

function AiSettings() {
  const { settings, update } = useSettings(useShallow((s) => ({ settings: s.settings, update: s.update })));
  const [providers, setProviders] = useState<ProviderInfo[]>([]);
  const [hasKey, setHasKey] = useState<boolean | null>(null);
  const [key, setKey] = useState('');
  const [model, setModel] = useState(settings.ai_model);
  const [endpoint, setEndpoint] = useState(settings.ai_endpoint);
  const [models, setModels] = useState<string[]>([]);
  const [loading, setLoading] = useState(false);
  const provider = settings.ai_provider;
  const info = providers.find((p) => p.provider === provider);
  const custom = provider === 'custom';

  useEffect(() => {
    api.aiProviders().then(setProviders).catch(() => setProviders([]));
  }, []);
  // Each provider has its own key and model list.
  useEffect(() => {
    setKey('');
    setModels([]);
    setModel(settings.ai_model);
    api
      .aiKeyStatus(provider)
      .then(setHasKey)
      .catch(() => setHasKey(false));
  }, [provider, settings.ai_model]);

  const choose = (next: AiProvider) => void update({ ai_provider: next, ai_model: '' });
  const saveKey = async (value: string | null) => {
    try {
      await api.aiSetKey(provider, value);
      setHasKey(Boolean(value));
      setKey('');
      useUi.getState().toast(value ? `${info?.name ?? 'API'} key saved in the keychain.` : 'API key removed.', 'positive');
    } catch (e) {
      useUi.getState().toastError(e);
    }
  };
  /** Lists the models the saved key can use; doubles as a key test. */
  const loadModels = async () => {
    setLoading(true);
    try {
      const found = await api.aiModels();
      setModels(found);
      useUi.getState().toast(found.length ? `${found.length} models available — pick one in Model.` : 'The provider listed no chat models.', found.length ? 'positive' : 'info');
    } catch (e) {
      useUi.getState().toastError(e);
    } finally {
      setLoading(false);
    }
  };

  return (
    <>
      <PageHead eyebrow="Settings" title="AI assistant" />
      <Card title="Assistant" flush footer="Generated SQL is placed in the editor and never runs by itself. Writes still ask for confirmation when you run them.">
        <div className={styles.check}>
          <Checkbox
            label="Turn on the AI assistant"
            help="Off by default, and DataQuery includes no AI service of its own: you bring an API key from the provider you choose. When on, an Assistant button appears in query tabs. Each request lists what it sends: your question and table/column names and types — never credentials, rows or results."
            checked={settings.ai_enabled}
            onChange={(v) => void update({ ai_enabled: v })}
          />
        </div>
        <div className={styles.fields}>
          <SelectField
            label="Provider"
            value={provider}
            onChange={(e) => choose(e.target.value as AiProvider)}
            options={(providers.length ? providers : [{ provider, name: provider } as ProviderInfo]).map((p) => ({ value: p.provider, label: p.name }))}
            hint={custom ? 'Any server with an OpenAI-compatible chat API: Ollama, LM Studio, OpenRouter, a company gateway…' : info?.key_url ? `Create a key at ${info.key_url}` : undefined}
          />
          {custom ? (
            <TextField
              label="Endpoint"
              mono
              placeholder="http://localhost:11434/v1"
              hint="Base URL; DataQuery calls /chat/completions and /models under it. A key is only sent over https:// (or to this computer)."
              value={endpoint}
              onChange={(e) => setEndpoint(e.target.value)}
              onBlur={() => endpoint !== settings.ai_endpoint && void update({ ai_endpoint: endpoint.trim() })}
            />
          ) : (
            <span />
          )}
          <TextField
            className={styles.wide}
            label={custom ? 'API key (optional)' : `${info?.name ?? 'Provider'} API key`}
            type="password"
            mono
            autoComplete="off"
            placeholder={hasKey ? '•••••••• (saved in the keychain)' : custom ? 'none' : 'Paste your API key'}
            hint="Stored in the system keychain for this provider only, never in the settings file or logs."
            value={key}
            onChange={(e) => setKey(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && key.trim() && void saveKey(key.trim())}
            action={
              <>
                <Button disabled={!key.trim()} onClick={() => void saveKey(key.trim())}>
                  Save key
                </Button>
                {hasKey && (
                  <Button variant="plain" onClick={() => void saveKey(null)}>
                    Remove
                  </Button>
                )}
              </>
            }
          />
          <TextField
            className={styles.wide}
            label="Model"
            mono
            list="dq-ai-models"
            placeholder={info?.default_model ?? 'Load models, then pick one'}
            hint={info?.default_model ? `Leave empty for ${info.default_model}.` : 'Required: use Load models to see what your key can use, or type a model id.'}
            value={model}
            onChange={(e) => setModel(e.target.value)}
            onBlur={() => model.trim() !== settings.ai_model && void update({ ai_model: model.trim() })}
            action={
              <Button busy={loading} disabled={!custom && !hasKey} onClick={() => void loadModels()} title="Asks the provider which models this key can use (also checks the key)">
                Load models
              </Button>
            }
          />
          <datalist id="dq-ai-models">
            {models.map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
        </div>
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

type CapabilityKey = keyof Capabilities;

/** Capabilities still to come for an engine that otherwise ships. */
const PLANNED: Partial<Record<Engine, CapabilityKey[]>> = {
  snowflake: ['monitoring'],
};

const NO_SERVER = 'An embedded database file: there are no other sessions to watch.';
const NO_PLANNER = 'There is no query planner to explain.';
const SCHEMALESS = 'There is no fixed schema to compare.';

/** Why a database does not offer a capability. Unlisted gaps fall back to a generic note. */
const NOT_OFFERED: Partial<Record<Engine, Partial<Record<CapabilityKey, string>>>> = {
  clickhouse: {
    grid_edit: 'ClickHouse has no row-level updates or unique keys that identify one row.',
    transactions: 'ClickHouse has no multi-statement transactions.',
  },
  sqlite: { monitoring: NO_SERVER },
  redis: { explain: NO_PLANNER, schema_diff: SCHEMALESS },
  valkey: { explain: NO_PLANNER, schema_diff: SCHEMALESS },
  dragonfly: { explain: NO_PLANNER, schema_diff: SCHEMALESS },
  mongodb: { schema_diff: SCHEMALESS },
  dynamodb: {
    explain: 'DynamoDB does not show query plans.',
    monitoring: 'DynamoDB exposes no server sessions or running queries.',
    schema_diff: SCHEMALESS,
  },
  cassandra: { transactions: 'Cassandra has no multi-statement transactions, only single-statement lightweight ones.' },
  scylladb: { transactions: 'ScyllaDB has no multi-statement transactions, only single-statement lightweight ones.' },
  elasticsearch: { transactions: 'Elasticsearch has no transactions.', schema_diff: 'Index mappings are not a relational schema.' },
  neo4j: {
    grid_edit: 'Graph results have no row identity; change data with Cypher in the editor.',
    schema_diff: 'Graphs have no fixed schema to compare.',
  },
};

function CapabilityCell({ engine, name, capability, supported, available, priority }: {
  engine: Engine;
  name: string;
  capability: { key: CapabilityKey; label: string };
  supported: boolean;
  available: boolean;
  priority: string;
}) {
  if (available && supported) {
    return (
      <span className={styles.yes} aria-label="Supported">
        ●
      </span>
    );
  }
  if (!available) return <span className={styles.planned}>{priority === 'MVP' ? 'P0' : priority}</span>;
  if (PLANNED[engine]?.includes(capability.key)) {
    return (
      <span className={styles.planned} title={`${capability.label} for ${name} is on the roadmap.`}>
        Planned
      </span>
    );
  }
  const reason = NOT_OFFERED[engine]?.[capability.key] ?? `${name} does not offer this.`;
  return (
    <span className={styles.no} title={reason} aria-label={`Not offered: ${reason}`}>
      —
    </span>
  );
}

/** Capability matrix (screen 15). Built-in engines report their runtime capabilities. */
function Engines() {
  const engines = useConnections((s) => s.engines);
  return (
    <>
      <PageHead eyebrow="Settings" title="Engines" />
      <Card
        title="Capabilities"
        flush
        footer="● shipped in this build · — not offered by that database (hover for why) · Planned: on the roadmap. Controls for unsupported capabilities are hidden for that engine."
      >
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
                      <CapabilityCell engine={e.engine} name={e.name} capability={c} supported={Boolean(e.capabilities[c.key])} available={e.available} priority={e.priority} />
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
      </Card>
      <UpdatesCard />
    </>
  );
}
