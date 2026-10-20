import { describe, expect, it } from 'vitest';

import type { ForeignConnection } from '@/bindings/ForeignConnection';

import { appName, importable, initialSelection, selectedConnections } from './importApps';

const row = (name: string, extra: Partial<ForeignConnection> = {}, username = 'app'): ForeignConnection => ({
  connection: {
    name,
    folder: null,
    environment: 'Local',
    favorite: false,
    config: {
      engine: 'postgres',
      host: 'db.local',
      port: 5432,
      database: 'app',
      username,
      ssl_mode: 'prefer',
      connect_timeout_secs: 10,
      tls_files: { ca_file: null, client_cert_file: null, client_key_file: null },
      ssh: null,
      read_only: false,
      options: { keepalive_secs: 60, connect_retries: 0, idle_timeout_mins: 0, pool_size: 4 },
      auth: { method: 'password' },
      properties: {},
    },
  },
  problem: null,
  duplicate_of: null,
  ...extra,
});

describe('import from other apps', () => {
  const rows = [
    row('ok'),
    row('duplicate', { duplicate_of: 'Existing' }),
    row('no user', { problem: 'Username is required.' }, ''),
    row('broken', { problem: 'Host contains characters that are not allowed.' }),
  ];

  it('selects valid rows that are not saved yet', () => {
    expect([...initialSelection(rows)]).toEqual([0]);
  });

  it('lets a typed username fix a missing one, and nothing else', () => {
    expect(importable(rows[2]!)).toBe(false);
    expect(importable(rows[2]!, ' ada ')).toBe(true);
    expect(importable(rows[3]!, 'ada')).toBe(false);
  });

  it('returns the selected rows with typed usernames', () => {
    const picked = selectedConnections(rows, new Set([0, 1, 2, 3]), { 2: ' ada ' });
    expect(picked.map((c) => [c.name, c.config.username])).toEqual([
      ['ok', 'app'],
      ['duplicate', 'app'],
      ['no user', 'ada'],
    ]);
    expect(selectedConnections(rows, new Set([2]), {})).toEqual([]);
  });

  it('names the apps', () => {
    expect(appName('sequel_ace')).toBe('Sequel Ace');
  });
});
