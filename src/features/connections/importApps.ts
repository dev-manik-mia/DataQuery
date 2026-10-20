import type { ConnectionImport } from '@/bindings/ConnectionImport';
import type { ForeignConnection } from '@/bindings/ForeignConnection';
import type { SourceApp } from '@/bindings/SourceApp';

export interface SourceAppInfo {
  app: SourceApp;
  name: string;
  /** What to choose when picking the file by hand. */
  file: string;
  extensions: string[];
}

export const SOURCE_APPS: SourceAppInfo[] = [
  {
    app: 'tableplus',
    name: 'TablePlus',
    file: 'Connections.plist, or an export saved without a password',
    extensions: ['plist', 'tableplusconnection', 'json'],
  },
  { app: 'dbeaver', name: 'DBeaver', file: 'data-sources.json in the project’s .dbeaver folder', extensions: ['json'] },
  { app: 'sequel_ace', name: 'Sequel Ace', file: 'Favorites.plist', extensions: ['plist'] },
  { app: 'sequel_pro', name: 'Sequel Pro', file: 'Favorites.plist', extensions: ['plist'] },
  {
    app: 'datagrip',
    name: 'DataGrip / JetBrains',
    file: 'dataSources.xml in the project’s .idea folder',
    extensions: ['xml'],
  },
];

export const appName = (app: SourceApp): string => SOURCE_APPS.find((a) => a.app === app)?.name ?? app;

/** Usernames typed into the preview, by row index. */
export type Usernames = Record<number, string>;

/** The row's only gap is a missing username, which the user can type in. */
export const needsUsername = (row: ForeignConnection): boolean =>
  row.problem !== null && row.connection.config.username.trim() === '';

/** A row can be imported when it is valid as read, or when its missing username was typed in. */
export function importable(row: ForeignConnection, username = ''): boolean {
  return row.problem === null || (needsUsername(row) && username.trim() !== '');
}

/** Valid rows that are not saved yet start selected. */
export function initialSelection(rows: ForeignConnection[]): Set<number> {
  return new Set(rows.flatMap((row, i) => (importable(row) && row.duplicate_of === null ? [i] : [])));
}

/** The selected rows as connections to create, with typed usernames applied. */
export function selectedConnections(
  rows: ForeignConnection[],
  selected: Set<number>,
  usernames: Usernames,
): ConnectionImport[] {
  return rows.flatMap((row, i) => {
    const username = usernames[i]?.trim() ?? '';
    if (!selected.has(i) || !importable(row, username)) return [];
    const { connection } = row;
    return [needsUsername(row) ? { ...connection, config: { ...connection.config, username } } : connection];
  });
}
