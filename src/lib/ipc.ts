/**
 * Typed wrappers for every backend command. Argument keys are camelCase
 * (Tauri maps them to the Rust snake_case parameters); payload types come
 * from `src/bindings`, which is generated from the Rust types.
 */
import { invoke } from '@tauri-apps/api/core';

import type { AppInfo } from '@/bindings/AppInfo';
import type { ApplyReport } from '@/bindings/ApplyReport';
import type { ColumnMapping } from '@/bindings/ColumnMapping';
import type { Confirmation } from '@/bindings/Confirmation';
import type { ConnectionDraft } from '@/bindings/ConnectionDraft';
import type { ConnectionList } from '@/bindings/ConnectionList';
import type { CsvOptions } from '@/bindings/CsvOptions';
import type { CsvPreview } from '@/bindings/CsvPreview';
import type { EngineInfo } from '@/bindings/EngineInfo';
import type { ExportFormat } from '@/bindings/ExportFormat';
import type { ExportReport } from '@/bindings/ExportReport';
import type { ExportSource } from '@/bindings/ExportSource';
import type { Folder } from '@/bindings/Folder';
import type { Guarded } from '@/bindings/Guarded';
import type { HistoryPage } from '@/bindings/HistoryPage';
import type { HistoryQuery } from '@/bindings/HistoryQuery';
import type { ImportReport } from '@/bindings/ImportReport';
import type { KeychainStatus } from '@/bindings/KeychainStatus';
import type { Node } from '@/bindings/Node';
import type { ObjectRef } from '@/bindings/ObjectRef';
import type { ParsedUrl } from '@/bindings/ParsedUrl';
import type { RowChange } from '@/bindings/RowChange';
import type { RowCount } from '@/bindings/RowCount';
import type { RowsPage } from '@/bindings/RowsPage';
import type { RunReport } from '@/bindings/RunReport';
import type { SavedConnection } from '@/bindings/SavedConnection';
import type { SavedScript } from '@/bindings/SavedScript';
import type { ScriptDraft } from '@/bindings/ScriptDraft';
import type { ServerInfo } from '@/bindings/ServerInfo';
import type { SessionInfo } from '@/bindings/SessionInfo';
import type { Settings } from '@/bindings/Settings';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import type { StatementInfo } from '@/bindings/StatementInfo';
import type { TablePage } from '@/bindings/TablePage';
import type { TablePageRequest } from '@/bindings/TablePageRequest';
import type { TableStructure } from '@/bindings/TableStructure';

import { toAppError } from './errors';

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (error) {
    throw toAppError(error);
  }
}

/** Strips UI-only fields so only an `ObjectRef` crosses the boundary. */
export function objectRef(o: ObjectRef): ObjectRef {
  return { kind: o.kind, database: o.database, schema: o.schema, name: o.name };
}

type Sid = { sessionId: string };

export const api = {
  // system
  appInfo: () => call<AppInfo>('app_info'),
  listEngines: () => call<EngineInfo[]>('list_engines'),
  keychainStatus: () => call<KeychainStatus>('keychain_status'),
  analyzeSql: (sql: string, flavor: SqlFlavor) => call<StatementInfo[]>('analyze_sql', { sql, flavor }),
  formatSql: (sql: string, flavor: SqlFlavor) => call<string>('format_sql', { sql, flavor }),
  parseUrl: (url: string) => call<ParsedUrl>('parse_url', { url }),
  readSqlFile: (path: string) => call<string>('read_sql_file', { path }),
  writeSqlFile: (path: string, contents: string) => call<void>('write_sql_file', { path, contents }),

  // settings and UI state
  getSettings: () => call<Settings>('get_settings'),
  saveSettings: (settings: Settings) => call<Settings>('save_settings', { settings }),
  getUiState: <T>(key: string) => call<T | null>('get_ui_state', { key }),
  setUiState: (key: string, value: unknown) => call<void>('set_ui_state', { key, value }),

  // connections
  listConnections: () => call<ConnectionList>('list_connections'),
  saveConnection: (draft: ConnectionDraft) => call<SavedConnection>('save_connection', { draft }),
  deleteConnection: (id: string) => call<void>('delete_connection', { id }),
  duplicateConnection: (id: string) => call<SavedConnection>('duplicate_connection', { id }),
  setFavorite: (id: string, favorite: boolean) => call<void>('set_favorite', { id, favorite }),
  createFolder: (name: string) => call<Folder>('create_folder', { name }),
  renameFolder: (id: string, name: string) => call<void>('rename_folder', { id, name }),
  deleteFolder: (id: string) => call<void>('delete_folder', { id }),
  testConnection: (draft: ConnectionDraft) => call<ServerInfo>('test_connection', { draft }),

  // sessions
  connect: (connectionId: string) => call<SessionInfo>('connect', { connectionId }),
  disconnect: ({ sessionId }: Sid) => call<void>('disconnect', { sessionId }),

  // explorer
  listChildren: ({ sessionId }: Sid, parent: ObjectRef | null) =>
    call<Node[]>('list_children', { sessionId, parent: parent && objectRef(parent) }),
  searchObjects: ({ sessionId }: Sid, query: string) => call<Node[]>('search_objects', { sessionId, query }),
  qualifiedName: ({ sessionId }: Sid, object: ObjectRef) =>
    call<string>('qualified_name', { sessionId, object: objectRef(object) }),
  generateSelect: ({ sessionId }: Sid, object: ObjectRef) =>
    call<string>('generate_select', { sessionId, object: objectRef(object) }),

  // queries
  runSql: ({ sessionId }: Sid, sql: string, confirmation: Confirmation | null) =>
    call<Guarded<RunReport>>('run_sql', { sessionId, sql, confirmation }),
  fetchMore: ({ sessionId }: Sid, limit: number) => call<RowsPage>('fetch_more', { sessionId, limit }),
  cancelQuery: ({ sessionId }: Sid) => call<boolean>('cancel_query', { sessionId }),

  // tables
  tableStructure: ({ sessionId }: Sid, object: ObjectRef) =>
    call<TableStructure>('table_structure', { sessionId, object: objectRef(object) }),
  tableDdl: ({ sessionId }: Sid, object: ObjectRef) => call<string>('table_ddl', { sessionId, object: objectRef(object) }),
  tablePage: ({ sessionId }: Sid, request: TablePageRequest) =>
    call<TablePage>('table_page', { sessionId, request: { ...request, object: objectRef(request.object) } }),
  rowCount: ({ sessionId }: Sid, object: ObjectRef, filter: string | null) =>
    call<RowCount>('row_count', { sessionId, object: objectRef(object), filter }),
  previewChanges: ({ sessionId }: Sid, object: ObjectRef, changes: RowChange[]) =>
    call<string>('preview_changes', { sessionId, object: objectRef(object), changes }),
  applyChanges: ({ sessionId }: Sid, object: ObjectRef, changes: RowChange[], confirmation: Confirmation | null) =>
    call<Guarded<ApplyReport>>('apply_changes', { sessionId, object: objectRef(object), changes, confirmation }),

  // import / export
  exportData: ({ sessionId }: Sid, source: ExportSource, format: ExportFormat, path: string) =>
    call<ExportReport>('export_data', { sessionId, source, format, path }),
  csvPreview: (path: string, options: CsvOptions) => call<CsvPreview>('csv_preview', { path, options }),
  importCsv: (
    { sessionId }: Sid,
    object: ObjectRef,
    path: string,
    options: CsvOptions,
    mapping: ColumnMapping[],
    confirmation: Confirmation | null,
  ) =>
    call<Guarded<ImportReport>>('import_csv', {
      sessionId,
      object: objectRef(object),
      path,
      options,
      mapping,
      confirmation,
    }),

  // history and scripts
  listHistory: (query: HistoryQuery) => call<HistoryPage>('list_history', { query }),
  clearHistory: () => call<void>('clear_history'),
  listScripts: () => call<SavedScript[]>('list_scripts'),
  saveScript: (draft: ScriptDraft) => call<SavedScript>('save_script', { draft }),
  deleteScript: (id: string) => call<void>('delete_script', { id }),
};
