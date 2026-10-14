import { create } from 'zustand';

import type { ExportFormat } from '@/bindings/ExportFormat';
import type { ExportSource } from '@/bindings/ExportSource';

export const EXPORT_FILTERS: Record<ExportFormat, { name: string; extensions: string[] }> = {
  csv: { name: 'CSV', extensions: ['csv'] },
  json: { name: 'JSON', extensions: ['json'] },
  xlsx: { name: 'Excel workbook', extensions: ['xlsx'] },
  sql_insert: { name: 'SQL', extensions: ['sql'] },
};

interface ExportRequest {
  source: ExportSource;
  format: ExportFormat;
  baseName: string;
}

/** The export the dialog is configuring (one at a time). */
export const useExportRequest = create<{ request: ExportRequest | null; close: () => void }>((set) => ({
  request: null,
  close: () => set({ request: null }),
}));

/**
 * Opens the export dialog for a source. Query exports re-run the statement on
 * a read-only connection without the row limit; table and query exports stream to disk.
 */
export function exportData(source: ExportSource, format: ExportFormat, baseName: string): void {
  useExportRequest.setState({ request: { source, format, baseName } });
}
