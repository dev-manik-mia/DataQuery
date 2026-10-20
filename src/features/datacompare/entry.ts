import type { CompareTable } from '@/bindings/CompareTable';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import { useUi } from '@/stores/ui';

/**
 * The single entry point of data compare. Navigation, the command palette and
 * the explorer menu all ask here, so availability (e.g. a plan gate) is decided
 * in one place.
 */
export function dataCompareAvailable(): boolean {
  return true;
}

/** Engines whose tables data compare can read: SQL databases (not CQL or document stores). */
export function comparesData(flavor: SqlFlavor | undefined): boolean {
  return flavor != null && !['mongodb', 'partiql', 'redis', 'cql', 'query_dsl', 'cypher'].includes(flavor);
}

/** Opens the data compare page, optionally with the source table filled in. */
export function openDataCompare(source?: CompareTable): void {
  if (!dataCompareAvailable()) return;
  useUi.getState().navigate({ name: 'data-compare', source });
}
