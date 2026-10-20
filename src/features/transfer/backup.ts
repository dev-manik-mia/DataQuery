import { create } from 'zustand';

import type { BackupFamily } from '@/bindings/BackupFamily';
import type { BackupFormat } from '@/bindings/BackupFormat';
import type { BackupOptions } from '@/bindings/BackupOptions';
import type { Tool } from '@/bindings/Tool';

export type BackupMode = 'backup' | 'restore';

interface BackupRequest {
  mode: BackupMode;
  database: string;
  /** The schema the dialog was opened on (PostgreSQL): backs up only that schema. */
  schema: string | null;
}

/** The backup or restore the dialog is configuring (one at a time). */
export const useBackupRequest = create<{ request: BackupRequest | null; close: () => void }>((set) => ({
  request: null,
  close: () => set({ request: null }),
}));

/** Opens the backup or restore dialog for a database (or one schema of it). */
export function openBackup(mode: BackupMode, database: string, schema: string | null = null): void {
  useBackupRequest.setState({ request: { mode, database, schema } });
}

export const FORMATS: Record<BackupFormat, { label: string; extension: string | null; hint: string }> = {
  custom: { label: 'Custom archive (pg_restore)', extension: 'dump', hint: 'Compressed; restore everything or parts with pg_restore.' },
  plain: { label: 'SQL script', extension: 'sql', hint: 'Plain SQL, readable and editable; restored with the command-line client.' },
  directory: { label: 'Directory (one file per table)', extension: null, hint: 'A folder that pg_dump creates; choose a name for it.' },
  tar: { label: 'Tar archive', extension: 'tar', hint: 'Uncompressed archive for pg_restore.' },
  archive: { label: 'mongodump archive', extension: 'archive', hint: 'All collections in one file, restored with mongorestore.' },
  file: { label: 'Database file copy', extension: 'sqlite', hint: 'A consistent copy made while the database stays usable.' },
};

/** The program that writes or reads backups of each format, for the dialog's tool line. */
export function toolsFor(family: BackupFamily, mode: BackupMode, format: BackupFormat | null): Tool[] {
  switch (family) {
    case 'postgres':
      if (mode === 'backup') return ['pg_dump'];
      return format === 'plain' ? ['psql'] : format ? ['pg_restore'] : ['pg_restore', 'psql'];
    case 'mysql':
      return [mode === 'backup' ? 'mysqldump' : 'mysql'];
    case 'mongodb':
      return [mode === 'backup' ? 'mongodump' : 'mongorestore'];
    case 'sqlite':
      return [];
  }
}

/** Defaults per family: what TablePlus and the tools' own defaults do. */
export function defaultOptions(family: BackupFamily, format: BackupFormat, database: string, schema: string | null): BackupOptions {
  return {
    database,
    format,
    content: 'all',
    schemas: family === 'postgres' && schema ? [schema] : [],
    tables: [],
    no_owner: false,
    clean: family === 'mysql',
    single_transaction: true,
    routines: true,
    triggers: true,
    events: false,
    compress: true,
  };
}

/** `app-2026-10-20-1430.dump`: the database, the local time and the format's extension. */
export function backupFileName(database: string, format: BackupFormat, gzip: boolean, now = new Date()): string {
  const pad = (n: number) => String(n).padStart(2, '0');
  const stamp = `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}-${pad(now.getHours())}${pad(now.getMinutes())}`;
  const base = `${(database.split(/[\\/]/).pop() || 'backup').replace(/\.[^.]*$/, '').replace(/[^\w.-]+/g, '_')}-${stamp}`;
  const extension = FORMATS[format].extension;
  if (!extension) return base;
  return `${base}.${extension}${format === 'archive' && gzip ? '.gz' : ''}`;
}

/** Names typed into one field, separated by commas or new lines. */
export function nameList(text: string): string[] {
  return text
    .split(/[,\n]/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/** The format a restore file probably has, from its name; the backend checks the contents. */
export function guessFormat(family: BackupFamily, path: string): BackupFormat | null {
  if (family !== 'postgres') return null;
  if (/\.sql$/i.test(path)) return 'plain';
  if (/\.tar$/i.test(path)) return 'tar';
  if (/\.(dump|backup|custom)$/i.test(path)) return 'custom';
  return null;
}
