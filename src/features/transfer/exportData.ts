import { save } from '@tauri-apps/plugin-dialog';

import type { ExportFormat } from '@/bindings/ExportFormat';
import type { ExportSource } from '@/bindings/ExportSource';
import { formatBytes, formatCount, formatDuration } from '@/lib/format';
import { api } from '@/lib/ipc';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

const FILTERS: Record<ExportFormat, { name: string; extensions: string[] }> = {
  csv: { name: 'CSV', extensions: ['csv'] },
  json: { name: 'JSON', extensions: ['json'] },
};

/**
 * Asks for a destination and streams the source to disk. Query exports re-run
 * the statement on a read-only connection without the row limit.
 */
export async function exportData(source: ExportSource, format: ExportFormat, baseName: string): Promise<void> {
  const session = useWorkspace.getState().session;
  if (!session) return;
  const path = await save({ defaultPath: `${baseName.replace(/[^\w.-]+/g, '_')}.${format}`, filters: [FILTERS[format]] });
  if (!path) return;
  const ui = useUi.getState();
  ui.toast(`Exporting to ${path.split(/[\\/]/).pop()}…`);
  try {
    const report = await api.exportData({ sessionId: session.session_id }, source, format, path);
    ui.toast(`Exported ${formatCount(report.rows)} rows (${formatBytes(report.bytes)}) in ${formatDuration(report.duration_ms)}.`, 'positive');
  } catch (e) {
    ui.toastError(e);
  }
}
