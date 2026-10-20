import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { SavedScript } from '@/bindings/SavedScript';
import { useUi } from '@/stores/ui';
import { useWorkspace, type QueryTab } from '@/stores/workspace';

import { onScriptsChanged } from './scriptConflicts';

const script: SavedScript = { id: 's1', name: 'daily', sql: 'select 2', connection_id: null, created_at: 0, updated_at: 0, folder: null, tags: [], pinned: false };
const api = vi.hoisted(() => ({ listScripts: vi.fn(), updateScriptSql: vi.fn(), saveScript: vi.fn() }));
vi.mock('@/lib/ipc', () => ({ api }));

/** A tab opened from script s1 (saved as `select 1`), with `sql` typed in it. */
function openTab(sql: string): string {
  const id = useWorkspace.getState().openQueryTab({ sql: 'select 1', title: 'daily', scriptId: 's1' });
  useWorkspace.getState().patchQueryTab(id, { sql });
  return id;
}

const tab = (id: string) => useWorkspace.getState().tabs.find((t) => t.id === id) as QueryTab;

describe('scripts changed on disk', () => {
  beforeEach(() => {
    useWorkspace.setState({ tabs: [], activeTabId: null });
    api.listScripts.mockResolvedValue([script]);
    api.updateScriptSql.mockResolvedValue(undefined);
    api.saveScript.mockResolvedValue({ ...script, id: 's2', name: 'daily (copy)' });
  });

  it('reloads a tab without unsaved edits', async () => {
    const id = openTab('select 1');
    await onScriptsChanged(['s1']);
    expect(tab(id)).toMatchObject({ sql: 'select 2', savedSql: 'select 2' });
  });

  it('asks before replacing unsaved edits', async () => {
    const id = openTab('select mine');
    const choose = vi.fn().mockResolvedValue('mine');
    useUi.setState({ choose });
    await onScriptsChanged(['s1']);
    expect(choose).toHaveBeenCalledOnce();
    expect(api.updateScriptSql).toHaveBeenCalledWith('s1', 'select mine', 'select 2');
    expect(tab(id)).toMatchObject({ sql: 'select mine', savedSql: 'select mine' });

    useWorkspace.getState().patchQueryTab(id, { sql: 'select mine 2' });
    useUi.setState({ choose: vi.fn().mockResolvedValue('copy') });
    api.listScripts.mockResolvedValue([{ ...script, sql: 'select 3' }]);
    await onScriptsChanged(['s1']);
    expect(tab(id)).toMatchObject({ scriptId: 's2', title: 'daily (copy)', sql: 'select mine 2' });
  });

  it('unlinks a tab whose script file was deleted, keeping its text unsaved', async () => {
    const id = openTab('select 1');
    api.listScripts.mockResolvedValue([]);
    await onScriptsChanged(['s1']);
    expect(tab(id)).toMatchObject({ scriptId: null, savedSql: '', sql: 'select 1' });
  });
});
