import { useCallback, useEffect, useMemo, useState } from 'react';

import type { AccountList } from '@/bindings/AccountList';
import type { AccountPrivileges } from '@/bindings/AccountPrivileges';
import type { AccountRef } from '@/bindings/AccountRef';
import type { DbError } from '@/bindings/DbError';
import type { Grant } from '@/bindings/Grant';
import type { GrantedPrivilege } from '@/bindings/GrantedPrivilege';
import { Alert, Button, Card, Checkbox, Chip, EmptyState, ErrorAlert, Input, PageHead, Spinner } from '@/components/ui';
import { t } from '@/i18n';
import { toAppError } from '@/lib/errors';
import { api } from '@/lib/ipc';
import { useWorkspace } from '@/stores/workspace';

import {
  AccountDialog,
  DropDialog,
  FLAG_LABEL,
  GrantDialog,
  LEVEL_LABEL,
  MembershipDialog,
  PasswordDialog,
  RenameDialog,
  RevokeRoleDialog,
  accountLabel,
} from './forms';
import styles from './Users.module.css';

type Form =
  | { kind: 'create'; role: boolean }
  | { kind: 'alter' | 'password' | 'rename' | 'drop' | 'member' }
  | { kind: 'grant'; initial?: Grant; revoke?: boolean }
  | { kind: 'revoke-role'; role: AccountRef };

const same = (a: AccountRef, b: AccountRef) => a.name === b.name && a.host === b.host;

function Privileges({ list }: { list: GrantedPrivilege[] }) {
  return (
    <span className={styles.chips}>
      {list.map((p) => (
        <Chip
          key={p.privilege}
          tone={p.grantable ? 'accent' : 'mute'}
          small
          title={p.grantable ? 'With grant option' : undefined}
        >
          {p.privilege}
          {p.grantable ? ' +' : ''}
        </Chip>
      ))}
    </span>
  );
}

/** `db.schema.table`, `db` or `*.*`, as the grant names its object. */
function grantObject(g: Grant): string {
  return g.level === 'server' ? '*.*' : [g.database, g.schema, g.object].filter(Boolean).join('.');
}

/** Users and roles of the server: list, attributes, privileges and the forms that change them. */
export function UsersView({ active }: { active: boolean }) {
  const session = useWorkspace((s) => s.session)!;
  const features = session.users;
  const sid = { sessionId: session.session_id };
  const [list, setList] = useState<AccountList | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  const [selected, setSelected] = useState<AccountRef | null>(null);
  const [privileges, setPrivileges] = useState<AccountPrivileges | null>(null);
  const [privError, setPrivError] = useState<DbError | null>(null);
  const [database, setDatabase] = useState(session.server.database);
  const [search, setSearch] = useState('');
  const [system, setSystem] = useState(false);
  const [form, setForm] = useState<Form | null>(null);
  const [loaded, setLoaded] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setList(await api.userAccounts(sid));
      setError(null);
    } catch (e) {
      setError(toAppError(e).info);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.session_id]);

  useEffect(() => {
    if (!active || loaded) return;
    setLoaded(true);
    void refresh();
  }, [active, loaded, refresh]);

  const account = list?.accounts.find((a) => selected && same(a, selected)) ?? null;

  const loadPrivileges = useCallback(async () => {
    if (!selected) return;
    setPrivileges(null);
    setPrivError(null);
    try {
      setPrivileges(await api.userPrivileges(sid, selected, database.trim()));
    } catch (e) {
      setPrivError(toAppError(e).info);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.session_id, selected, database]);

  useEffect(() => {
    if (!active || !selected) return;
    const timer = window.setTimeout(() => void loadPrivileges(), 200);
    return () => window.clearTimeout(timer);
  }, [active, selected, loadPrivileges]);

  const shown = useMemo(() => {
    const q = search.trim().toLowerCase();
    return (list?.accounts ?? []).filter(
      (a) => (system || !a.system || a.current) && (!q || accountLabel(a).toLowerCase().includes(q)),
    );
  }, [list, search, system]);
  const roles = (list?.accounts ?? []).filter((a) => a.role && !a.system);

  if (!features) {
    return (
      <div className={styles.page} style={active ? undefined : { display: 'none' }}>
        <EmptyState title={t('nav.users')}>
          User and role management is available for PostgreSQL, CockroachDB, MySQL and MariaDB.
        </EmptyState>
      </div>
    );
  }

  const applied = () => {
    void refresh();
    void loadPrivileges();
  };
  const close = () => setForm(null);
  const common = { features, onClose: close, onApplied: applied };
  const readOnly = session.connection.config.read_only;
  const roleModel = features.model === 'role';

  return (
    <div className={styles.page} style={active ? undefined : { display: 'none' }}>
      <div className={styles.inner}>
        <PageHead
          eyebrow={`Administration · ${session.server.version}`}
          title={t('nav.users')}
          actions={
            <>
              <Button size="sm" disabled={readOnly} onClick={() => setForm({ kind: 'create', role: false })}>
                New user
              </Button>
              {features.roles && (
                <Button size="sm" disabled={readOnly} onClick={() => setForm({ kind: 'create', role: true })}>
                  New role
                </Button>
              )}
              <Button size="sm" onClick={() => (void refresh(), void loadPrivileges())}>
                Refresh
              </Button>
            </>
          }
        />
        {readOnly && (
          <Alert tone="info">This connection is read-only: accounts can be inspected but not changed.</Alert>
        )}
        {session.connection.environment === 'Production' && (
          <Alert tone="caution">Production: every change asks for confirmation; drops need the connection name.</Alert>
        )}
        {error && <ErrorAlert error={error} />}
        {list && list.unavailable.length > 0 && (
          <Alert tone="info" title="Some information is not available">
            {list.unavailable.map((u) => (
              <div key={u}>{u}</div>
            ))}
          </Alert>
        )}
        {!list && !error && <Spinner label="Reading accounts" />}
        {list && (
          <div className={styles.split}>
            <Card
              title={`Accounts (${shown.length})`}
              flush
              actions={
                <Input
                  label="Filter accounts"
                  placeholder="Filter"
                  value={search}
                  onChange={(e) => setSearch(e.target.value)}
                />
              }
              footer={<Checkbox label="Show system accounts" checked={system} onChange={setSystem} />}
            >
              <div className={styles.list} role="list">
                {shown.map((a) => (
                  <button
                    key={accountLabel(a)}
                    type="button"
                    role="listitem"
                    className={styles.item}
                    aria-current={selected !== null && same(a, selected)}
                    onClick={() => setSelected({ name: a.name, host: a.host })}
                  >
                    <span className="mono ellipsis">{accountLabel(a)}</span>
                    <span className={styles.chips}>
                      <Chip tone={a.role ? 'faint' : 'positive'} small>
                        {a.role ? 'role' : 'user'}
                      </Chip>
                      {a.flags.includes('superuser') && (
                        <Chip tone="critical" small>
                          superuser
                        </Chip>
                      )}
                      {a.locked && (
                        <Chip tone="caution" small>
                          locked
                        </Chip>
                      )}
                      {a.current && (
                        <Chip tone="accent" small>
                          you
                        </Chip>
                      )}
                    </span>
                  </button>
                ))}
                {shown.length === 0 && <div className={styles.item}>No matching accounts.</div>}
              </div>
            </Card>
            <div className={styles.detail}>
              {!account ? (
                <EmptyState title="Choose an account">
                  Its attributes, role memberships and privileges show here.
                </EmptyState>
              ) : (
                <>
                  <Card
                    title={<span className="mono">{accountLabel(account)}</span>}
                    actions={
                      <span className={styles.actions}>
                        <Button size="sm" disabled={readOnly} onClick={() => setForm({ kind: 'alter' })}>
                          Edit…
                        </Button>
                        {!(account.role && !roleModel) && (
                          <Button size="sm" disabled={readOnly} onClick={() => setForm({ kind: 'password' })}>
                            Password…
                          </Button>
                        )}
                        {features.rename && (
                          <Button size="sm" disabled={readOnly} onClick={() => setForm({ kind: 'rename' })}>
                            Rename…
                          </Button>
                        )}
                        <Button size="sm" disabled={readOnly} onClick={() => setForm({ kind: 'grant' })}>
                          Grant…
                        </Button>
                        {features.roles && (
                          <Button size="sm" disabled={readOnly} onClick={() => setForm({ kind: 'member' })}>
                            Add to role…
                          </Button>
                        )}
                        <Button
                          size="sm"
                          variant="danger"
                          disabled={readOnly}
                          onClick={() => setForm({ kind: 'drop' })}
                        >
                          Drop…
                        </Button>
                      </span>
                    }
                  >
                    <dl className={styles.facts}>
                      <dt>Kind</dt>
                      <dd>
                        {account.role ? 'Role' : 'User'}
                        {account.system ? ' · built in' : ''}
                        {account.current ? ' · this connection' : ''}
                      </dd>
                      {features.flags.length > 0 && (
                        <>
                          <dt>Attributes</dt>
                          <dd>
                            {features.flags
                              .filter((f) => account.flags.includes(f))
                              .map((f) => FLAG_LABEL[f])
                              .join(', ') || 'none'}
                          </dd>
                        </>
                      )}
                      {features.connection_limit && (
                        <>
                          <dt>Connection limit</dt>
                          <dd>{account.connection_limit ?? 'none'}</dd>
                        </>
                      )}
                      {features.valid_until && (
                        <>
                          <dt>Password valid until</dt>
                          <dd>{account.valid_until ?? 'no expiry'}</dd>
                        </>
                      )}
                      {account.locked !== null && (
                        <>
                          <dt>Account</dt>
                          <dd>
                            {account.locked ? 'Locked' : 'Unlocked'}
                            {account.password_expired ? ' · password expired' : ''}
                          </dd>
                        </>
                      )}
                      {account.auth_plugin && (
                        <>
                          <dt>Authentication</dt>
                          <dd className="mono">{account.auth_plugin}</dd>
                        </>
                      )}
                      {account.limits && (
                        <>
                          <dt>Limits</dt>
                          <dd>
                            {`${account.limits.max_queries_per_hour || '∞'} queries/h · ${account.limits.max_updates_per_hour || '∞'} updates/h · ${account.limits.max_connections_per_hour || '∞'} connections/h · ${account.limits.max_user_connections || '∞'} concurrent`}
                          </dd>
                        </>
                      )}
                    </dl>
                  </Card>
                  {features.roles && (
                    <Card title={`Member of (${account.member_of.length})`} flush>
                      {account.member_of.length === 0 && <div className={styles.row}>Not a member of any role.</div>}
                      {account.member_of.map((r) => (
                        <div key={accountLabel(r)} className={styles.row}>
                          <span>Role</span>
                          <span className="mono ellipsis">{accountLabel(r)}</span>
                          <span />
                          <Button
                            size="xs"
                            disabled={readOnly}
                            onClick={() => setForm({ kind: 'revoke-role', role: r })}
                          >
                            Remove…
                          </Button>
                        </div>
                      ))}
                    </Card>
                  )}
                  <Card
                    title="Privileges"
                    flush
                    actions={
                      roleModel ? (
                        <Input
                          label="Database whose schemas and tables are listed"
                          mono
                          value={database}
                          onChange={(e) => setDatabase(e.target.value)}
                        />
                      ) : undefined
                    }
                    footer={`Chips marked + carry the grant option.${roleModel ? ' Schema and table privileges are listed for the database above; privileges inherited from roles are listed on the roles.' : ''}`}
                  >
                    {privError && (
                      <div style={{ padding: 12 }}>
                        <ErrorAlert error={privError} />
                      </div>
                    )}
                    {!privileges && !privError && (
                      <div className={styles.row}>
                        <Spinner />
                      </div>
                    )}
                    {privileges && (
                      <>
                        <div className={`${styles.row} ${styles.head}`}>
                          <span>Level</span>
                          <span>Object</span>
                          <span>Privileges</span>
                          <span />
                        </div>
                        {privileges.grants.length === 0 && (
                          <div className={styles.row}>No privileges granted directly.</div>
                        )}
                        {privileges.grants.map((g) => (
                          <div key={`${g.level}:${grantObject(g)}`} className={styles.row}>
                            <span>{LEVEL_LABEL[g.level]}</span>
                            <span className="mono ellipsis" title={g.object_kind ?? undefined}>
                              {grantObject(g)}
                            </span>
                            <Privileges list={g.privileges} />
                            <Button
                              size="xs"
                              disabled={readOnly}
                              onClick={() => setForm({ kind: 'grant', initial: g, revoke: true })}
                            >
                              Revoke…
                            </Button>
                          </div>
                        ))}
                      </>
                    )}
                  </Card>
                  {privileges && privileges.unavailable.length > 0 && (
                    <Alert tone="info" title="Some privileges could not be read">
                      {privileges.unavailable.map((u) => (
                        <div key={u}>{u}</div>
                      ))}
                    </Alert>
                  )}
                  {privileges && privileges.default_privileges.length > 0 && (
                    <Card
                      title="Default privileges"
                      flush
                      footer="Granted automatically on objects these owners create later (ALTER DEFAULT PRIVILEGES)."
                    >
                      {privileges.default_privileges.map((d) => (
                        <div key={`${d.owner}:${d.schema}:${d.object_type}`} className={styles.row}>
                          <span>{d.object_type}</span>
                          <span className="mono ellipsis">
                            {d.schema ?? 'all schemas'} · by {d.owner}
                          </span>
                          <Privileges list={d.privileges} />
                          <span />
                        </div>
                      ))}
                    </Card>
                  )}
                  {privileges && privileges.owned.length > 0 && (
                    <Card
                      title="Owned objects"
                      flush
                      footer="DROP ROLE fails while the role owns objects or holds privileges in any database."
                    >
                      {privileges.owned.map((o) => (
                        <div key={o.database ?? ''} className={styles.row}>
                          <span>{o.database ? 'Database' : 'Server'}</span>
                          <span className="mono ellipsis">{o.database ?? 'databases, tablespaces'}</span>
                          <span>
                            {o.owned} owned · {o.privileges} with privileges
                          </span>
                          <span />
                        </div>
                      ))}
                    </Card>
                  )}
                  {privileges && privileges.statements.length > 0 && (
                    <Card title="SHOW GRANTS" flush>
                      <pre className={`${styles.lines} mono`}>{privileges.statements.join('\n')}</pre>
                    </Card>
                  )}
                </>
              )}
            </div>
          </div>
        )}
      </div>
      {form?.kind === 'create' && <AccountDialog {...common} role={form.role} roles={roles} />}
      {account && form?.kind === 'alter' && <AccountDialog {...common} account={account} roles={roles} />}
      {account && form?.kind === 'password' && <PasswordDialog {...common} account={selected!} />}
      {account && form?.kind === 'rename' && (
        <RenameDialog onClose={close} onApplied={() => (setSelected(null), applied())} account={selected!} />
      )}
      {account && form?.kind === 'drop' && (
        <DropDialog
          {...common}
          onApplied={() => (setSelected(null), void refresh())}
          account={account}
          owned={privileges?.owned ?? []}
          others={(list?.accounts ?? []).filter((a) => !same(a, account) && !a.system)}
        />
      )}
      {account && form?.kind === 'grant' && (
        <GrantDialog {...common} account={selected!} initial={form.initial} revoke={form.revoke} />
      )}
      {account && form?.kind === 'member' && (
        <MembershipDialog onClose={close} onApplied={applied} account={account} roles={roles} />
      )}
      {account && form?.kind === 'revoke-role' && (
        <RevokeRoleDialog onClose={close} onApplied={applied} account={selected!} role={form.role} />
      )}
    </div>
  );
}
