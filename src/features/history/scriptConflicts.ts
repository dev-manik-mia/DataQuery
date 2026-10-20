/**
 * Open tabs whose saved script changed outside the tab — usually its `.sql`
 * file in the scripts folder after a `git pull` or an edit in another editor.
 * A tab without unsaved edits follows the new text; a tab with unsaved edits
 * asks first. Nothing is overwritten without the user choosing it.
 */
import type { SavedScript } from '@/bindings/SavedScript';
import { api } from '@/lib/ipc';
import { useUi } from '@/stores/ui';
import { isDirty, useWorkspace, type QueryTab } from '@/stores/workspace';

const asking = new Set<string>();

function queryTab(id: string): QueryTab | undefined {
  return useWorkspace.getState().tabs.find((t): t is QueryTab => t.id === id && t.kind === 'query');
}

/** Brings open tabs up to date after scripts `ids` changed on disk. */
export async function onScriptsChanged(ids: string[]): Promise<void> {
  const linked = useWorkspace.getState().tabs.filter((t): t is QueryTab => t.kind === 'query' && t.scriptId !== null && ids.includes(t.scriptId));
  if (!linked.length) return;
  const scripts = await api.listScripts();
  const { patchQueryTab } = useWorkspace.getState();
  for (const { id } of linked) {
    const tab = queryTab(id);
    if (!tab?.scriptId) continue;
    const script = scripts.find((s) => s.id === tab.scriptId);
    if (!script) {
      // Its file was deleted: the tab keeps its text, unsaved, so closing it still asks.
      patchQueryTab(tab.id, { scriptId: null, savedSql: '' });
      useUi.getState().toast(`${tab.title} was deleted from the scripts folder. The tab keeps its SQL.`);
    } else if (script.sql !== tab.savedSql) {
      if (isDirty(tab)) await resolveScriptConflict(tab.id, script);
      else patchQueryTab(tab.id, { sql: script.sql, savedSql: script.sql, title: script.name });
    }
  }
}

/** Asks what to do with a tab whose script changed while it has unsaved edits. */
export async function resolveScriptConflict(tabId: string, known?: SavedScript): Promise<void> {
  const first = queryTab(tabId);
  if (!first?.scriptId || asking.has(tabId)) return;
  asking.add(tabId);
  const ui = useUi.getState();
  try {
    const script = known ?? (await api.listScripts()).find((s) => s.id === first.scriptId);
    if (!script) return;
    const choice = await ui.choose({
      title: `${script.name} changed on disk`,
      body: 'The script was changed outside this tab (for example in the scripts folder) while the tab has unsaved edits.',
      choices: [
        { value: 'copy', label: 'Save mine as a copy' },
        { value: 'mine', label: 'Keep mine' },
        { value: 'reload', label: 'Reload', variant: 'primary' },
      ],
    });
    const tab = queryTab(tabId);
    if (!tab || !choice) return;
    const { patchQueryTab } = useWorkspace.getState();
    if (choice === 'reload') {
      patchQueryTab(tab.id, { sql: script.sql, savedSql: script.sql, title: script.name });
    } else if (choice === 'mine') {
      await api.updateScriptSql(script.id, tab.sql, script.sql);
      patchQueryTab(tab.id, { savedSql: tab.sql });
    } else {
      const copy = await api.saveScript({ ...script, id: null, name: `${script.name} (copy)`, sql: tab.sql, pinned: false });
      patchQueryTab(tab.id, { scriptId: copy.id, title: copy.name, savedSql: tab.sql });
      ui.toast(`Saved your version as ${copy.name}.`, 'positive');
    }
  } catch (e) {
    ui.toastError(e);
  } finally {
    asking.delete(tabId);
  }
}
