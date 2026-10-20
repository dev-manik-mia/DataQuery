import { open, save } from '@tauri-apps/plugin-dialog';

import type { ConnectionImportReport } from '@/bindings/ConnectionImportReport';
import { plural } from '@/lib/format';
import { api } from '@/lib/ipc';
import { useConnections } from '@/stores/connections';
import { useUi } from '@/stores/ui';

const JSON_FILTER = [{ name: 'DataQuery connections', extensions: ['json'] }];

/** Writes connections (all, or `ids`) to a JSON file. Secrets are never included. */
export async function exportConnections(ids: string[] | null): Promise<void> {
  const path = await save({ defaultPath: 'dataquery-connections.json', filters: JSON_FILTER });
  if (!path) return;
  try {
    const count = await api.exportConnections(path, ids);
    useUi.getState().toast(`Exported ${plural(count, 'connection')} without passwords.`, 'positive');
  } catch (e) {
    useUi.getState().toastError(e);
  }
}

/** Reloads the list and reports what an import created and skipped. */
export async function reportConnectionImport(report: ConnectionImportReport): Promise<void> {
  await useConnections.getState().load();
  const ui = useUi.getState();
  ui.toast(`Imported ${plural(report.imported, 'connection')}. Enter their passwords before connecting.`, report.skipped.length ? 'info' : 'positive');
  if (report.skipped.length) ui.toast(`Skipped: ${report.skipped.join('; ')}`, 'critical');
}

/** Imports connections from an export file. Passwords must be entered again. */
export async function importConnections(): Promise<void> {
  const path = await open({ multiple: false, filters: JSON_FILTER });
  if (typeof path !== 'string') return;
  try {
    await reportConnectionImport(await api.importConnections(path));
  } catch (e) {
    useUi.getState().toastError(e);
  }
}
