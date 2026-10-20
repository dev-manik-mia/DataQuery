import { open, save } from '@tauri-apps/plugin-dialog';

import { resolveScriptConflict } from '@/features/history/scriptConflicts';
import { api } from '@/lib/ipc';
import { useUi } from '@/stores/ui';
import { useWorkspace, type QueryTab } from '@/stores/workspace';

const SQL_FILTER = [{ name: 'SQL', extensions: ['sql', 'txt'] }];

function fileName(path: string): string {
  return path.split(/[\\/]/).pop() ?? path;
}

/** Opens `.sql` files in new tabs. Nothing runs until the user presses Run. */
export async function openSqlFiles(paths?: string[]): Promise<void> {
  const chosen = paths ?? (await open({ multiple: true, filters: SQL_FILTER }));
  const list = Array.isArray(chosen) ? chosen : chosen ? [chosen] : [];
  for (const path of list) {
    try {
      const sql = await api.readSqlFile(path);
      useWorkspace.getState().openQueryTab({ sql, title: fileName(path), filePath: path });
    } catch (e) {
      useUi.getState().toastError(e);
    }
  }
}

export async function saveTabToFile(tab: QueryTab, saveAs = false): Promise<boolean> {
  const path = !saveAs && tab.filePath ? tab.filePath : await save({ defaultPath: tab.filePath ?? `${tab.title}.sql`, filters: SQL_FILTER });
  if (!path) return false;
  try {
    await api.writeSqlFile(path, tab.sql);
    useWorkspace.getState().markSaved(tab.id, { filePath: path, title: fileName(path) });
    useUi.getState().toast(`Saved ${fileName(path)}`, 'positive');
    return true;
  } catch (e) {
    useUi.getState().toastError(e);
    return false;
  }
}

export async function saveTabAsScript(tab: QueryTab, name: string): Promise<void> {
  const session = useWorkspace.getState().session;
  try {
    // Keep the folder, tags and pin of an existing script.
    const existing = tab.scriptId ? (await api.listScripts()).find((s) => s.id === tab.scriptId) : undefined;
    // Changed elsewhere (e.g. its file in the scripts folder) since this tab loaded it: ask, never overwrite.
    if (existing && existing.sql !== tab.savedSql && existing.sql !== tab.sql) {
      await resolveScriptConflict(tab.id, existing);
      return;
    }
    const script = await api.saveScript({
      id: existing?.id ?? null,
      name,
      sql: tab.sql,
      connection_id: existing?.connection_id ?? session?.connection.id ?? null,
      folder: existing?.folder ?? null,
      tags: existing?.tags ?? [],
      pinned: existing?.pinned ?? false,
    });
    useWorkspace.getState().markSaved(tab.id, { scriptId: script.id, title: script.name });
    useUi.getState().toast(`Saved script ${script.name}`, 'positive');
  } catch (e) {
    useUi.getState().toastError(e);
  }
}
