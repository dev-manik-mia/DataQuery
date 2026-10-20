import { useMemo, useState, type ReactNode } from 'react';

import type { Account } from '@/bindings/Account';
import type { AccountAttributes } from '@/bindings/AccountAttributes';
import type { AccountRef } from '@/bindings/AccountRef';
import type { Grant } from '@/bindings/Grant';
import type { GrantLevel } from '@/bindings/GrantLevel';
import type { GrantTarget } from '@/bindings/GrantTarget';
import type { OwnedObjects } from '@/bindings/OwnedObjects';
import type { ResourceLimits } from '@/bindings/ResourceLimits';
import type { RoleFlag } from '@/bindings/RoleFlag';
import type { UserAdminFeatures } from '@/bindings/UserAdminFeatures';
import type { UserChange } from '@/bindings/UserChange';
import { Alert, Checkbox, Segmented, SelectField, TextField } from '@/components/ui';
import { SqlReviewDialog } from '@/features/schema/SqlReviewDialog';
import { api } from '@/lib/ipc';
import { useWorkspace } from '@/stores/workspace';

import styles from './Users.module.css';

/** `name` or `'name'@'host'`, as the user reads it. */
export function accountLabel(a: AccountRef): string {
  return a.host === null ? a.name : `'${a.name}'@'${a.host}'`;
}

export const FLAG_LABEL: Record<RoleFlag, string> = {
  login: 'Can log in',
  superuser: 'Superuser',
  createdb: 'Create databases',
  createrole: 'Create roles',
  inherit: 'Inherit privileges',
  replication: 'Replication',
  bypassrls: 'Bypass row security',
};

export const LEVEL_LABEL: Record<GrantLevel, string> = {
  server: 'Server',
  database: 'Database',
  schema: 'Schema',
  schema_tables: 'All tables in schema',
  table: 'Table',
};

const LIMIT_LABEL: Record<keyof ResourceLimits, string> = {
  max_queries_per_hour: 'Queries per hour',
  max_updates_per_hour: 'Updates per hour',
  max_connections_per_hour: 'Connections per hour',
  max_user_connections: 'Concurrent connections',
};

const NO_LIMITS: ResourceLimits = {
  max_queries_per_hour: 0,
  max_updates_per_hour: 0,
  max_connections_per_hour: 0,
  max_user_connections: 0,
};

const NO_ATTRIBUTES: AccountAttributes = {
  flags: [],
  connection_limit: null,
  valid_until: null,
  locked: null,
  expire_password: false,
  limits: null,
};

/** Common props of every form: what the server supports and how to finish. */
interface FormProps {
  features: UserAdminFeatures;
  onClose: () => void;
  onApplied: () => void;
}

/**
 * A user change through the shared form → reviewed SQL → apply flow. The
 * reviewed SQL masks passwords, so it cannot be opened in an editor then.
 */
function UserChangeDialog({
  title,
  change,
  children,
  onClose,
  onApplied,
  direct,
}: {
  title: string;
  change: UserChange | string;
  children?: ReactNode;
  onClose: () => void;
  onApplied: () => void;
  direct?: boolean;
}) {
  const sid = { sessionId: useWorkspace((s) => s.session)!.session_id };
  const ready = typeof change !== 'string';
  const secret = ready && (change.op === 'create' || change.op === 'set_password') && change.password !== null;
  return (
    <SqlReviewDialog
      title={title}
      missing={ready ? null : change}
      preview={() => (ready ? api.userChangePreview(sid, change) : Promise.reject(new Error(change)))}
      apply={(c) => (ready ? api.applyUserChange(sid, change, c) : Promise.reject(new Error(change)))}
      done={(n) => `Applied ${n} statement${n === 1 ? '' : 's'}.`}
      onClose={onClose}
      onApplied={onApplied}
      direct={direct}
      editable={!secret}
      fileName="user-change.sql"
      note={secret ? 'The password is shown as *** and is never saved or added to history.' : undefined}
    >
      {children}
    </SqlReviewDialog>
  );
}

/** A whole number typed into a form: `undefined` when empty, `NaN` when invalid. */
function parseWhole(text: string): number | undefined {
  const t = text.trim();
  return t === '' ? undefined : /^-?\d+$/.test(t) ? Number(t) : NaN;
}

function PasswordFields({
  password,
  confirm,
  onPassword,
  onConfirm,
}: {
  password: string;
  confirm: string;
  onPassword: (v: string) => void;
  onConfirm: (v: string) => void;
}) {
  return (
    <div className={styles.fields}>
      <TextField
        label="Password"
        type="password"
        autoComplete="new-password"
        value={password}
        onChange={(e) => onPassword(e.target.value)}
      />
      <TextField
        label="Repeat password"
        type="password"
        autoComplete="new-password"
        value={confirm}
        onChange={(e) => onConfirm(e.target.value)}
        error={confirm && confirm !== password ? 'The passwords differ.' : undefined}
      />
    </div>
  );
}

// ------------------------------------------------------------------ create / alter

/**
 * Create a user or role, or change an account's attributes. When altering,
 * only what differs from the account is sent.
 */
export function AccountDialog({
  features,
  account,
  role: asRole = false,
  roles,
  onClose,
  onApplied,
}: FormProps & { account?: Account; role?: boolean; roles: Account[] }) {
  const userHost = features.model === 'user_host';
  const creating = !account;
  const roleOnly = userHost && (account?.role ?? asRole);
  const [name, setName] = useState('');
  const [host, setHost] = useState('%');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const initialFlags = (f: RoleFlag) =>
    account ? account.flags.includes(f) : f === 'login' ? !asRole : f === 'inherit';
  const [flags, setFlags] = useState<Record<string, boolean>>(() =>
    Object.fromEntries(features.flags.map((f) => [f, initialFlags(f)])),
  );
  const [limit, setLimit] = useState(account?.connection_limit != null ? String(account.connection_limit) : '');
  const [validUntil, setValidUntil] = useState(account?.valid_until ?? '');
  const [locked, setLocked] = useState(account?.locked ?? false);
  const [expire, setExpire] = useState(false);
  const [limits, setLimits] = useState<Record<keyof ResourceLimits, string>>(() => {
    const l = account?.limits ?? NO_LIMITS;
    return {
      max_queries_per_hour: String(l.max_queries_per_hour),
      max_updates_per_hour: String(l.max_updates_per_hour),
      max_connections_per_hour: String(l.max_connections_per_hour),
      max_user_connections: String(l.max_user_connections),
    };
  });
  const [memberOf, setMemberOf] = useState<string[]>([]);
  const target: AccountRef = account
    ? { name: account.name, host: account.host }
    : { name: name.trim(), host: userHost && !asRole ? host.trim() : null };

  const change = useMemo<UserChange | string>(() => {
    if (creating && !target.name) return `Name the ${asRole ? 'role' : 'user'}.`;
    if (password !== confirm) return 'The passwords differ.';
    const attributes: AccountAttributes = {
      ...NO_ATTRIBUTES,
      flags: features.flags
        .filter((f) => flags[f] !== initialFlags(f) || (creating && flags[f]))
        .map((f) => ({ flag: f, on: Boolean(flags[f]) })),
    };
    if (features.connection_limit) {
      const n = parseWhole(limit);
      if (Number.isNaN(n)) return 'The connection limit must be a whole number.';
      const current = account?.connection_limit ?? undefined;
      if (n !== current && !(creating && n === undefined)) attributes.connection_limit = n ?? -1;
    }
    if (features.valid_until && validUntil.trim() !== (account?.valid_until ?? ''))
      attributes.valid_until = validUntil.trim() || 'infinity';
    if (features.lock && !roleOnly && locked !== (account?.locked ?? false)) attributes.locked = locked;
    if (features.expire_password && !roleOnly) attributes.expire_password = expire;
    if (features.resource_limits && !roleOnly) {
      const parsed = Object.fromEntries(
        Object.entries(limits).map(([k, v]) => [k, parseWhole(v) ?? 0]),
      ) as ResourceLimits;
      if (Object.values(parsed).some((v) => Number.isNaN(v) || v < 0))
        return 'Limits are whole numbers; 0 means unlimited.';
      const before = account?.limits ?? NO_LIMITS;
      if ((Object.keys(parsed) as (keyof ResourceLimits)[]).some((k) => parsed[k] !== Number(before[k])))
        attributes.limits = parsed;
    }
    if (creating) {
      return {
        op: 'create',
        account: target,
        role: asRole,
        password: password && !roleOnly ? password : null,
        attributes,
        member_of: roles.filter((r) => memberOf.includes(accountLabel(r))).map((r) => ({ name: r.name, host: r.host })),
      };
    }
    return { op: 'alter', account: target, attributes };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [name, host, password, confirm, flags, limit, validUntil, locked, expire, limits, memberOf]);

  const kind = asRole || account?.role ? 'role' : 'user';
  return (
    <UserChangeDialog
      title={creating ? `New ${kind}` : `Edit ${accountLabel(target)}`}
      change={change}
      onClose={onClose}
      onApplied={onApplied}
    >
      {creating && (
        <div className={styles.fields}>
          <TextField
            label={`${kind[0]!.toUpperCase()}${kind.slice(1)} name`}
            mono
            value={name}
            onChange={(e) => setName(e.target.value)}
            autoFocus
          />
          {userHost && !asRole && (
            <TextField
              label="Host"
              mono
              value={host}
              onChange={(e) => setHost(e.target.value)}
              hint="% matches any host; localhost only local connections."
            />
          )}
        </div>
      )}
      {creating && !roleOnly && (
        <PasswordFields password={password} confirm={confirm} onPassword={setPassword} onConfirm={setConfirm} />
      )}
      {features.flags.length > 0 && (
        <div className={styles.grid}>
          {features.flags.map((f) => (
            <Checkbox
              key={f}
              label={FLAG_LABEL[f]}
              checked={Boolean(flags[f])}
              onChange={(on) => setFlags((s) => ({ ...s, [f]: on }))}
            />
          ))}
        </div>
      )}
      {(features.connection_limit || features.valid_until) && (
        <div className={styles.fields}>
          {features.connection_limit && (
            <TextField
              label="Connection limit"
              mono
              value={limit}
              onChange={(e) => setLimit(e.target.value)}
              hint="Empty for no limit."
            />
          )}
          {features.valid_until && (
            <TextField
              label="Password valid until"
              mono
              placeholder="2030-12-31"
              value={validUntil}
              onChange={(e) => setValidUntil(e.target.value)}
              hint="Empty for no expiry."
            />
          )}
        </div>
      )}
      {features.resource_limits && !roleOnly && (
        <div className={styles.fields}>
          {(Object.keys(LIMIT_LABEL) as (keyof ResourceLimits)[]).map((k) => (
            <TextField
              key={k}
              label={LIMIT_LABEL[k]}
              mono
              value={limits[k]}
              onChange={(e) => setLimits((s) => ({ ...s, [k]: e.target.value }))}
            />
          ))}
        </div>
      )}
      {(features.lock || features.expire_password) && !roleOnly && (
        <div className={styles.grid}>
          {features.lock && <Checkbox label="Account locked" checked={locked} onChange={setLocked} />}
          {features.expire_password && (
            <Checkbox label="Expire the password now" checked={expire} onChange={setExpire} />
          )}
        </div>
      )}
      {creating && features.roles && roles.length > 0 && (
        <>
          <div className="dq-field__label" style={{ marginTop: 12 }}>
            Member of
          </div>
          <div className={styles.grid}>
            {roles.map((r) => {
              const label = accountLabel(r);
              return (
                <Checkbox
                  key={label}
                  label={label}
                  checked={memberOf.includes(label)}
                  onChange={(on) => setMemberOf((s) => (on ? [...s, label] : s.filter((x) => x !== label)))}
                />
              );
            })}
          </div>
        </>
      )}
    </UserChangeDialog>
  );
}

// ------------------------------------------------------------------ password, rename

export function PasswordDialog({ features, account, onClose, onApplied }: FormProps & { account: AccountRef }) {
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [none, setNone] = useState(false);
  const canClear = features.model === 'role';
  const change: UserChange | string = none
    ? { op: 'set_password', account, password: null }
    : !password
      ? 'Enter the new password.'
      : password !== confirm
        ? 'The passwords differ.'
        : { op: 'set_password', account, password };
  return (
    <UserChangeDialog
      title={`Password of ${accountLabel(account)}`}
      change={change}
      onClose={onClose}
      onApplied={onApplied}
    >
      {!none && (
        <PasswordFields password={password} confirm={confirm} onPassword={setPassword} onConfirm={setConfirm} />
      )}
      {canClear && (
        <div style={{ marginTop: 10 }}>
          <Checkbox label="No password (password logins stop working)" checked={none} onChange={setNone} />
        </div>
      )}
    </UserChangeDialog>
  );
}

export function RenameDialog({ account, onClose, onApplied }: Omit<FormProps, 'features'> & { account: AccountRef }) {
  const [name, setName] = useState(account.name);
  const change: UserChange | string =
    !name.trim() || name.trim() === account.name
      ? 'Enter a new name.'
      : { op: 'rename', account, new_name: name.trim() };
  return (
    <UserChangeDialog title={`Rename ${accountLabel(account)}`} change={change} onClose={onClose} onApplied={onApplied}>
      <TextField label="New name" mono value={name} onChange={(e) => setName(e.target.value)} autoFocus />
    </UserChangeDialog>
  );
}

// ------------------------------------------------------------------ drop

export function DropDialog({
  features,
  account,
  owned,
  others,
  onClose,
  onApplied,
}: FormProps & { account: Account; owned: OwnedObjects[]; others: Account[] }) {
  const database = useWorkspace((s) => s.session)!.server.database;
  const [reassign, setReassign] = useState('');
  const [dropOwned, setDropOwned] = useState(false);
  const elsewhere = owned.filter((o) => o.database !== database && (o.owned > 0 || o.privileges > 0));
  const change: UserChange = {
    op: 'drop',
    account: { name: account.name, host: account.host },
    role: account.role,
    reassign_to: reassign || null,
    drop_owned: dropOwned,
    database: features.owned_objects ? database : null,
  };
  return (
    <UserChangeDialog title={`Drop ${accountLabel(account)}`} change={change} onClose={onClose} onApplied={onApplied}>
      <Alert tone="caution">
        Dropping {account.role ? 'the role removes it from every member' : 'the user ends its access'}. This cannot be
        undone.
        {account.current && ' This is the account this connection uses.'}
      </Alert>
      {features.owned_objects && (
        <>
          {owned.length > 0 && (
            <Alert tone="info" title="Objects and privileges tie this role to databases">
              {owned.map((o) => (
                <div key={o.database ?? ''}>
                  {o.database ?? 'Server-wide (databases, tablespaces)'}: {o.owned} owned, {o.privileges} with
                  privileges
                </div>
              ))}
              {elsewhere.length > 0 && (
                <div>REASSIGN OWNED and DROP OWNED act on {database} only; run them in the other databases first.</div>
              )}
            </Alert>
          )}
          <div className={styles.fields}>
            <SelectField
              label={`Reassign objects in ${database} to`}
              value={reassign}
              onChange={(e) => setReassign(e.target.value)}
              options={[
                { value: '', label: 'Do not reassign' },
                ...others.map((o) => ({ value: o.name, label: o.name })),
              ]}
            />
          </div>
          <div style={{ marginTop: 10 }}>
            <Checkbox
              label={`Drop what it still owns in ${database} and revoke its privileges there (DROP OWNED)`}
              checked={dropOwned}
              onChange={setDropOwned}
            />
          </div>
        </>
      )}
    </UserChangeDialog>
  );
}

// ------------------------------------------------------------------ grant / revoke

/** The object fields a level needs. */
function targetOf(level: GrantLevel, database: string, schema: string, table: string): GrantTarget | string {
  const d = database.trim();
  const s = schema.trim();
  const t = table.trim();
  switch (level) {
    case 'server':
      return { level };
    case 'database':
      return d ? { level, database: d } : 'Enter the database.';
    case 'schema':
    case 'schema_tables':
      return d && s ? { level, database: d, schema: s } : 'Enter the database and schema.';
    case 'table':
      return d && t ? { level, database: d, schema: s || null, table: t } : 'Enter the database and table.';
  }
}

/**
 * Grant or revoke privileges on one object: a checkbox per privilege the
 * level allows, plus ALL. Revoking from a listed grant starts with its privileges.
 */
export function GrantDialog({
  features,
  account,
  initial,
  revoke: startRevoke = false,
  onClose,
  onApplied,
}: FormProps & { account: AccountRef; initial?: Grant; revoke?: boolean }) {
  const session = useWorkspace((s) => s.session)!;
  const roles = features.model === 'role';
  const [mode, setMode] = useState<'grant' | 'revoke'>(startRevoke ? 'revoke' : 'grant');
  const [level, setLevel] = useState<GrantLevel>(initial?.level ?? (roles ? 'table' : 'database'));
  const [database, setDatabase] = useState(initial?.database ?? session.server.database);
  const [schema, setSchema] = useState(initial?.schema ?? (roles ? 'public' : ''));
  const [table, setTable] = useState(initial?.object ?? '');
  const [chosen, setChosen] = useState<string[]>(initial?.privileges.map((p) => p.privilege) ?? []);
  const [option, setOption] = useState(false);
  const spec = features.levels.find((l) => l.level === level);
  const all = chosen.includes('ALL');
  const toggle = (p: string, on: boolean) => setChosen((s) => (on ? [...s, p] : s.filter((x) => x !== p)));

  const target = targetOf(level, database, schema, table);
  const privileges = all ? ['ALL'] : chosen.filter((p) => spec?.privileges.includes(p));
  const change: UserChange | string =
    typeof target === 'string'
      ? target
      : privileges.length === 0
        ? 'Choose at least one privilege.'
        : mode === 'grant'
          ? { op: 'grant', account, target, privileges, grant_option: option }
          : { op: 'revoke', account, target, privileges, grant_option_only: option };
  return (
    <UserChangeDialog
      title={`${mode === 'grant' ? 'Grant to' : 'Revoke from'} ${accountLabel(account)}`}
      change={change}
      onClose={onClose}
      onApplied={onApplied}
    >
      <div className={styles.actions}>
        <Segmented<'grant' | 'revoke'>
          label="Grant or revoke"
          value={mode}
          onChange={setMode}
          options={[
            { value: 'grant', label: 'Grant' },
            { value: 'revoke', label: 'Revoke' },
          ]}
        />
        <Segmented<GrantLevel>
          label="Level"
          value={level}
          onChange={setLevel}
          options={features.levels.map((l) => ({ value: l.level, label: LEVEL_LABEL[l.level] }))}
        />
      </div>
      <div className={styles.fields}>
        {level !== 'server' && (
          <TextField label="Database" mono value={database} onChange={(e) => setDatabase(e.target.value)} />
        )}
        {roles && level !== 'server' && level !== 'database' && (
          <TextField label="Schema" mono value={schema} onChange={(e) => setSchema(e.target.value)} />
        )}
        {level === 'table' && (
          <TextField label="Table or view" mono value={table} onChange={(e) => setTable(e.target.value)} />
        )}
      </div>
      <div className={styles.grid}>
        <Checkbox label="ALL" checked={all} onChange={(on) => toggle('ALL', on)} />
        {spec?.privileges.map((p) => (
          <Checkbox
            key={p}
            label={p}
            disabled={all}
            checked={all || chosen.includes(p)}
            onChange={(on) => toggle(p, on)}
          />
        ))}
      </div>
      <div style={{ marginTop: 12 }}>
        {mode === 'grant' ? (
          <Checkbox label="WITH GRANT OPTION (may grant these to others)" checked={option} onChange={setOption} />
        ) : (
          <Checkbox
            label={
              roles
                ? 'Only the grant option (GRANT OPTION FOR)'
                : 'Only the grant option (for every privilege on this object)'
            }
            checked={option}
            onChange={setOption}
          />
        )}
      </div>
    </UserChangeDialog>
  );
}

// ------------------------------------------------------------------ role membership

export function MembershipDialog({
  account,
  roles,
  onClose,
  onApplied,
}: Omit<FormProps, 'features'> & { account: Account; roles: Account[] }) {
  const choices = roles.filter(
    (r) =>
      !(r.name === account.name && r.host === account.host) &&
      !account.member_of.some((m) => m.name === r.name && m.host === r.host),
  );
  const [role, setRole] = useState(choices[0] ? accountLabel(choices[0]) : '');
  const [admin, setAdmin] = useState(false);
  const picked = choices.find((r) => accountLabel(r) === role);
  const change: UserChange | string = picked
    ? {
        op: 'grant_role',
        account: { name: account.name, host: account.host },
        role: { name: picked.name, host: picked.host },
        admin_option: admin,
      }
    : 'There is no other role to add.';
  return (
    <UserChangeDialog
      title={`Add ${accountLabel(account)} to a role`}
      change={change}
      onClose={onClose}
      onApplied={onApplied}
    >
      <SelectField
        label="Role"
        mono
        value={role}
        onChange={(e) => setRole(e.target.value)}
        options={choices.map((r) => ({ value: accountLabel(r), label: accountLabel(r) }))}
      />
      <div style={{ marginTop: 10 }}>
        <Checkbox label="WITH ADMIN OPTION (may add others to the role)" checked={admin} onChange={setAdmin} />
      </div>
    </UserChangeDialog>
  );
}

/** Revoking a role membership has no form: straight to the reviewed SQL. */
export function RevokeRoleDialog({
  account,
  role,
  onClose,
  onApplied,
}: Omit<FormProps, 'features'> & { account: AccountRef; role: AccountRef }) {
  return (
    <UserChangeDialog
      title={`Remove ${accountLabel(account)} from ${accountLabel(role)}`}
      change={{ op: 'revoke_role', account, role }}
      onClose={onClose}
      onApplied={onApplied}
      direct
    />
  );
}
