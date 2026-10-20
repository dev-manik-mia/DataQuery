import { useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import { Button, Dialog, ErrorAlert, TextField } from '@/components/ui';
import { toAppError } from '@/lib/errors';
import { api } from '@/lib/ipc';
import { useUi } from '@/stores/ui';

/** Parses `postgres://…`, `mysql://…`, `sqlserver://…`, `oracle://…` and similar URLs into the form. The password goes to the keychain on save, never the URL. */
export function ImportUrlDialog({ onClose }: { onClose: () => void }) {
  const navigate = useUi((s) => s.navigate);
  const [url, setUrl] = useState('');
  const [error, setError] = useState<DbError | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const parsed = await api.parseUrl(url);
      onClose();
      navigate({ name: 'connection-form', engine: parsed.config.engine, prefill: parsed });
    } catch (e) {
      setError(toAppError(e).info);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      title="Import connection URL"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" busy={busy} disabled={!url.trim()} onClick={() => void submit()}>
            Continue
          </Button>
        </>
      }
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <TextField
          label="Connection URL"
          mono
          type="password"
          autoFocus
          placeholder="postgres://user:password@host:5432/database"
          hint="postgres://, cockroachdb://, redshift://, greenplum://, mysql://, mariadb://, sqlserver://, oracle://, clickhouse://, snowflake://, vertica://, mongodb://, mongodb+srv://, redis://, valkey://, dragonfly://, neo4j://, neo4j+s://, libsql:// or duckdb:///path/to/file. The URL is not stored. A password in it is moved to the system keychain when you save."
          value={url}
          onChange={(e) => setUrl(e.target.value)}
        />
      </form>
      {error && <ErrorAlert error={error} />}
    </Dialog>
  );
}
