import { useCallback, useEffect, useState } from 'react';
import { useShallow } from 'zustand/react/shallow';

import type { ToolStatus } from '@/bindings/ToolStatus';
import { Button, Card, FileField } from '@/components/ui';
import { api } from '@/lib/ipc';
import { useSettings } from '@/stores/settings';

import styles from './Settings.module.css';

/**
 * The programs Backup and Restore run, with the version found. Each can be
 * pointed at a specific install, e.g. a newer pg_dump than the one on PATH.
 */
export function BackupToolsCard() {
  const { settings, update } = useSettings(useShallow((s) => ({ settings: s.settings, update: s.update })));
  const [tools, setTools] = useState<ToolStatus[] | null>(null);
  const reload = useCallback(() => {
    setTools(null);
    api.backupTools().then(setTools, () => setTools([]));
  }, []);
  useEffect(reload, [reload]);

  const choose = async (tool: string, path: string | null) => {
    const next = { ...settings.backup_tools };
    if (path) next[tool] = path;
    else delete next[tool];
    await update({ backup_tools: next });
    reload();
  };

  return (
    <Card
      title="Backup tools"
      footer="Found on PATH and in the usual install folders (Homebrew, Postgres.app, Program Files). The copy that suits the server is used unless you choose one here."
    >
      <div className={styles.fields}>
        {(tools ?? []).map((s) => (
          <FileField
            key={s.tool}
            label={<span className="mono">{s.tool}</span>}
            value={settings.backup_tools[s.tool] ?? null}
            onChange={(path) => void choose(s.tool, path)}
            hint={s.path ? `${s.custom ? 'Using' : 'Found'} ${s.path} · ${s.version ?? 'unknown version'}` : s.error}
            error={s.custom && s.error ? s.error : undefined}
          />
        ))}
      </div>
      <div style={{ marginTop: 10 }}>
        <Button size="sm" busy={tools === null} onClick={reload}>
          Look again
        </Button>
      </div>
    </Card>
  );
}
