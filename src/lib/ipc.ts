/**
 * Typed wrappers for every backend command. Argument keys are camelCase
 * (Tauri maps them to the Rust snake_case parameters); payload types come
 * from `src/bindings`, which is generated from the Rust types.
 */
import { invoke } from '@tauri-apps/api/core';

import type { AiAnswer } from '@/bindings/AiAnswer';
import type { AiDisclosure } from '@/bindings/AiDisclosure';
import type { AiProvider } from '@/bindings/AiProvider';
import type { AiRequest } from '@/bindings/AiRequest';
import type { AppInfo } from '@/bindings/AppInfo';
import type { BackupOptions } from '@/bindings/BackupOptions';
import type { BackupPlan } from '@/bindings/BackupPlan';
import type { BackupReport } from '@/bindings/BackupReport';
import type { ApplyReport } from '@/bindings/ApplyReport';
import type { Cell } from '@/bindings/Cell';
import type { ColumnMapping } from '@/bindings/ColumnMapping';
import type { ColumnValue } from '@/bindings/ColumnValue';
import type { Confirmation } from '@/bindings/Confirmation';
import type { ConnectionDraft } from '@/bindings/ConnectionDraft';
import type { ConnectionImportReport } from '@/bindings/ConnectionImportReport';
import type { ConnectionList } from '@/bindings/ConnectionList';
import type { Dependencies } from '@/bindings/Dependencies';
import type { DiffSide } from '@/bindings/DiffSide';
import type { DocIndex } from '@/bindings/DocIndex';
import type { DocumentChange } from '@/bindings/DocumentChange';
import type { DocumentPage } from '@/bindings/DocumentPage';
import type { EngineInfo } from '@/bindings/EngineInfo';
import type { ExplainResult } from '@/bindings/ExplainResult';
import type { ExportFormat } from '@/bindings/ExportFormat';
import type { ExportOptions } from '@/bindings/ExportOptions';
import type { ExportReport } from '@/bindings/ExportReport';
import type { ExportSource } from '@/bindings/ExportSource';
import type { FindRequest } from '@/bindings/FindRequest';
import type { Folder } from '@/bindings/Folder';
import type { FrameworkMigrations } from '@/bindings/FrameworkMigrations';
import type { Guarded } from '@/bindings/Guarded';
import type { HistoryPage } from '@/bindings/HistoryPage';
import type { HistoryQuery } from '@/bindings/HistoryQuery';
import type { ImportOptions } from '@/bindings/ImportOptions';
import type { ImportPreview } from '@/bindings/ImportPreview';
import type { ImportReport } from '@/bindings/ImportReport';
import type { ImportSource } from '@/bindings/ImportSource';
import type { ConsoleReply } from '@/bindings/ConsoleReply';
import type { KeychainStatus } from '@/bindings/KeychainStatus';
import type { KeyRole } from '@/bindings/KeyRole';
import type { KeyWrite } from '@/bindings/KeyWrite';
import type { MonitorSnapshot } from '@/bindings/MonitorSnapshot';
import type { Node } from '@/bindings/Node';
import type { ObjectRef } from '@/bindings/ObjectRef';
import type { ObjectSize } from '@/bindings/ObjectSize';
import type { ParsedUrl } from '@/bindings/ParsedUrl';
import type { ProviderInfo } from '@/bindings/ProviderInfo';
import type { QueryAnalysis } from '@/bindings/QueryAnalysis';
import type { RestoreOptions } from '@/bindings/RestoreOptions';
import type { RowChange } from '@/bindings/RowChange';
import type { RowCount } from '@/bindings/RowCount';
import type { RowFilter } from '@/bindings/RowFilter';
import type { RowsPage } from '@/bindings/RowsPage';
import type { RunReport } from '@/bindings/RunReport';
import type { SavedConnection } from '@/bindings/SavedConnection';
import type { ScanPage } from '@/bindings/ScanPage';
import type { ScanRequest } from '@/bindings/ScanRequest';
import type { SavedScript } from '@/bindings/SavedScript';
import type { SchemaApplyReport } from '@/bindings/SchemaApplyReport';
import type { SchemaChange } from '@/bindings/SchemaChange';
import type { SchemaDiff } from '@/bindings/SchemaDiff';
import type { SchemaModel } from '@/bindings/SchemaModel';
import type { ScriptDraft } from '@/bindings/ScriptDraft';
import type { ServerInfo } from '@/bindings/ServerInfo';
import type { ServerSetting } from '@/bindings/ServerSetting';
import type { SessionInfo } from '@/bindings/SessionInfo';
import type { SessionOption } from '@/bindings/SessionOption';
import type { Settings } from '@/bindings/Settings';
import type { Snippet } from '@/bindings/Snippet';
import type { SnippetDraft } from '@/bindings/SnippetDraft';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import type { StatementInfo } from '@/bindings/StatementInfo';
import type { TablePage } from '@/bindings/TablePage';
import type { TablePageRequest } from '@/bindings/TablePageRequest';
import type { TableStructure } from '@/bindings/TableStructure';
import type { TemplateKind } from '@/bindings/TemplateKind';
import type { ToolStatus } from '@/bindings/ToolStatus';
import type { TxAction } from '@/bindings/TxAction';
import type { TxStatus } from '@/bindings/TxStatus';
import type { ValuePage } from '@/bindings/ValuePage';
import type { ValueRequest } from '@/bindings/ValueRequest';

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
  return o.parent ? { kind: o.kind, database: o.database, schema: o.schema, name: o.name, parent: o.parent } : { kind: o.kind, database: o.database, schema: o.schema, name: o.name };
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
  trustHostKey: (host: string, port: number, fingerprint: string) => call<void>('trust_host_key', { host, port, fingerprint }),
  exportConnections: (path: string, ids: string[] | null) => call<number>('export_connections', { path, ids }),
  importConnections: (path: string) => call<ConnectionImportReport>('import_connections', { path }),

  // sessions
  connect: (connectionId: string) => call<SessionInfo>('connect', { connectionId }),
  reconnect: ({ sessionId }: Sid, connectionId: string) => call<SessionInfo>('reconnect', { sessionId, connectionId }),
  disconnect: ({ sessionId }: Sid) => call<void>('disconnect', { sessionId }),

  // explorer
  listChildren: ({ sessionId }: Sid, parent: ObjectRef | null) =>
    call<Node[]>('list_children', { sessionId, parent: parent && objectRef(parent) }),
  searchObjects: ({ sessionId }: Sid, query: string) => call<Node[]>('search_objects', { sessionId, query }),
  qualifiedName: ({ sessionId }: Sid, object: ObjectRef) =>
    call<string>('qualified_name', { sessionId, object: objectRef(object) }),
  generateSql: ({ sessionId }: Sid, object: ObjectRef, kind: TemplateKind) =>
    call<string>('generate_sql', { sessionId, object: objectRef(object), kind }),
  dependencies: ({ sessionId }: Sid, object: ObjectRef) =>
    call<Dependencies>('object_dependencies', { sessionId, object: objectRef(object) }),
  schemaModel: ({ sessionId }: Sid, database: string, schema: string | null) =>
    call<SchemaModel>('schema_model', { sessionId, database, schema }),

  // queries
  runSql: ({ sessionId }: Sid, sql: string, params: (string | null)[] | null, confirmation: Confirmation | null) =>
    call<Guarded<RunReport>>('run_sql', { sessionId, sql, params, confirmation }),
  queryAnalysis: ({ sessionId }: Sid, sql: string) => call<QueryAnalysis | null>('query_analysis', { sessionId, sql }),
  sessionOptions: ({ sessionId }: Sid) => call<SessionOption[]>('session_options', { sessionId }),
  setSessionOption: ({ sessionId }: Sid, name: string, value: string) => call<SessionOption[]>('set_session_option', { sessionId, name, value }),
  keyRoles: ({ sessionId }: Sid, object: ObjectRef) => call<KeyRole[]>('key_roles', { sessionId, object: objectRef(object) }),
  fetchMore: ({ sessionId }: Sid, limit: number) => call<RowsPage>('fetch_more', { sessionId, limit }),
  cancelQuery: ({ sessionId }: Sid) => call<boolean>('cancel_query', { sessionId }),
  transaction: ({ sessionId }: Sid, action: TxAction) => call<TxStatus>('transaction', { sessionId, action }),
  explain: ({ sessionId }: Sid, sql: string, params: (string | null)[] | null, analyze: boolean) =>
    call<ExplainResult>('explain', { sessionId, sql, params, analyze }),

  // tables
  tableStructure: ({ sessionId }: Sid, object: ObjectRef) =>
    call<TableStructure>('table_structure', { sessionId, object: objectRef(object) }),
  tableDdl: ({ sessionId }: Sid, object: ObjectRef) => call<string>('table_ddl', { sessionId, object: objectRef(object) }),
  tablePage: ({ sessionId }: Sid, request: TablePageRequest) =>
    call<TablePage>('table_page', { sessionId, request: { ...request, object: objectRef(request.object) } }),
  rowCount: ({ sessionId }: Sid, object: ObjectRef, rows: RowFilter) =>
    call<RowCount>('row_count', { sessionId, object: objectRef(object), rows }),
  /** `fullRow` matches rows of a table without a key on every comparable column. */
  previewChanges: ({ sessionId }: Sid, object: ObjectRef, changes: RowChange[], fullRow: boolean) =>
    call<string>('preview_changes', { sessionId, object: objectRef(object), changes, fullRow }),
  applyChanges: ({ sessionId }: Sid, object: ObjectRef, changes: RowChange[], fullRow: boolean, confirmation: Confirmation | null) =>
    call<Guarded<ApplyReport>>('apply_changes', { sessionId, object: objectRef(object), changes, fullRow, confirmation }),
  cellValue: ({ sessionId }: Sid, object: ObjectRef, key: ColumnValue[], column: string) =>
    call<Cell>('cell_value', { sessionId, object: objectRef(object), key, column }),
  saveCell: ({ sessionId }: Sid, object: ObjectRef, key: ColumnValue[], column: string, path: string) =>
    call<number>('save_cell', { sessionId, object: objectRef(object), key, column, path }),

  // documents (MongoDB, DynamoDB)
  documentsFind: ({ sessionId }: Sid, request: FindRequest) =>
    call<DocumentPage>('documents_find', { sessionId, request: { ...request, object: objectRef(request.object) } }),
  documentsCount: ({ sessionId }: Sid, object: ObjectRef, filter: string) =>
    call<RowCount | null>('documents_count', { sessionId, object: objectRef(object), filter }),
  documentsAggregate: ({ sessionId }: Sid, object: ObjectRef, pipeline: string, limit: number) =>
    call<DocumentPage>('documents_aggregate', { sessionId, object: objectRef(object), pipeline, limit }),
  documentsIndexes: ({ sessionId }: Sid, object: ObjectRef) => call<DocIndex[]>('documents_indexes', { sessionId, object: objectRef(object) }),
  documentsValidation: ({ sessionId }: Sid, object: ObjectRef) =>
    call<string | null>('documents_validation', { sessionId, object: objectRef(object) }),
  documentsStatement: ({ sessionId }: Sid, request: FindRequest) =>
    call<string>('documents_statement', { sessionId, request: { ...request, object: objectRef(request.object) } }),
  documentsWrite: ({ sessionId }: Sid, object: ObjectRef, change: DocumentChange, confirmation: Confirmation | null) =>
    call<Guarded<string>>('documents_write', { sessionId, object: objectRef(object), change, confirmation }),

  // keys (Redis, Valkey)
  keysScan: ({ sessionId }: Sid, request: ScanRequest) => call<ScanPage>('keys_scan', { sessionId, request }),
  keysCount: ({ sessionId }: Sid, database: number) => call<number>('keys_count', { sessionId, database }),
  keysValue: ({ sessionId }: Sid, request: ValueRequest) => call<ValuePage>('keys_value', { sessionId, request }),
  keysWrite: ({ sessionId }: Sid, write: KeyWrite, confirmation: Confirmation | null) =>
    call<Guarded<null>>('keys_write', { sessionId, write, confirmation }),
  keysConsole: ({ sessionId }: Sid, line: string, confirmation: Confirmation | null) =>
    call<Guarded<ConsoleReply>>('keys_console', { sessionId, line, confirmation }),

  // schema
  schemaChangePreview: ({ sessionId }: Sid, changes: SchemaChange[]) => call<string>('schema_change_preview', { sessionId, changes }),
  applySchemaChanges: ({ sessionId }: Sid, changes: SchemaChange[], confirmation: Confirmation | null) =>
    call<Guarded<SchemaApplyReport>>('apply_schema_changes', { sessionId, changes, confirmation }),
  schemaDiff: (source: DiffSide, target: DiffSide) => call<SchemaDiff>('schema_diff', { source, target }),
  frameworkMigrations: ({ sessionId }: Sid, database: string, schema: string | null) =>
    call<FrameworkMigrations[]>('framework_migrations', { sessionId, database, schema }),

  // monitoring
  monitorSnapshot: ({ sessionId }: Sid, longRunningSecs: number) => call<MonitorSnapshot>('monitor_snapshot', { sessionId, longRunningSecs }),
  serverSettings: ({ sessionId }: Sid, search: string | null) => call<ServerSetting[]>('server_settings', { sessionId, search }),
  largestTables: ({ sessionId }: Sid, database: string, limit: number) => call<ObjectSize[]>('largest_tables', { sessionId, database, limit }),
  killProcess: ({ sessionId }: Sid, pid: number, user: string | null, terminate: boolean, confirmation: Confirmation | null) =>
    call<Guarded<boolean>>('kill_process', { sessionId, pid, user, terminate, confirmation }),

  // import / export
  exportData: ({ sessionId }: Sid, source: ExportSource, format: ExportFormat, options: ExportOptions, path: string, jobId: string) =>
    call<ExportReport>('export_data', { sessionId, source, format, options, path, jobId }),
  importPreview: (source: ImportSource) => call<ImportPreview>('import_preview', { source }),
  importData: (
    { sessionId }: Sid,
    object: ObjectRef,
    source: ImportSource,
    mapping: ColumnMapping[],
    options: ImportOptions,
    jobId: string,
    confirmation: Confirmation | null,
  ) => call<Guarded<ImportReport>>('import_data', { sessionId, object: objectRef(object), source, mapping, options, jobId, confirmation }),
  cancelTransfer: (jobId: string) => call<boolean>('cancel_transfer', { jobId }),

  // backup / restore (cancelled with cancelTransfer)
  backupTools: () => call<ToolStatus[]>('backup_tools'),
  backupPlan: ({ sessionId }: Sid) => call<BackupPlan>('backup_plan', { sessionId }),
  backupDatabase: ({ sessionId }: Sid, options: BackupOptions, path: string, jobId: string) =>
    call<BackupReport>('backup_database', { sessionId, options, path, jobId }),
  restoreDatabase: ({ sessionId }: Sid, options: RestoreOptions, path: string, jobId: string, confirmation: Confirmation | null) =>
    call<Guarded<BackupReport>>('restore_database', { sessionId, options, path, jobId, confirmation }),

  // history, scripts, snippets
  listHistory: (query: HistoryQuery) => call<HistoryPage>('list_history', { query }),
  clearHistory: (connectionId: string | null) => call<void>('clear_history', { connectionId }),
  applyHistoryRetention: () => call<void>('apply_history_retention'),
  listScripts: () => call<SavedScript[]>('list_scripts'),
  saveScript: (draft: ScriptDraft) => call<SavedScript>('save_script', { draft }),
  updateScriptSql: (id: string, sql: string) => call<void>('update_script_sql', { id, sql }),
  deleteScript: (id: string) => call<void>('delete_script', { id }),
  listSnippets: () => call<Snippet[]>('list_snippets'),
  saveSnippet: (draft: SnippetDraft) => call<Snippet>('save_snippet', { draft }),
  deleteSnippet: (id: string) => call<void>('delete_snippet', { id }),

  // AI assistant (opt-in; never runs SQL)
  aiDisclosure: ({ sessionId }: Sid, request: AiRequest) => call<AiDisclosure>('ai_disclosure', { sessionId, request }),
  aiAsk: ({ sessionId }: Sid, request: AiRequest) => call<AiAnswer>('ai_ask', { sessionId, request }),
  aiProviders: () => call<ProviderInfo[]>('ai_providers'),
  aiModels: () => call<string[]>('ai_models'),
  aiKeyStatus: (provider: AiProvider) => call<boolean>('ai_key_status', { provider }),
  aiSetKey: (provider: AiProvider, key: string | null) => call<void>('ai_set_key', { provider, key }),
};

/** An empty row filter. */
export const NO_FILTER: RowFilter = { filter: null, matches: [], searches: [], conditions: [], match_any: false };
