import { useEffect, useState } from 'react';

import type { ObjectRef } from '@/bindings/ObjectRef';
import { api } from '@/lib/ipc';
import { keyBadges } from '@/lib/objects';
import { useWorkspace } from '@/stores/workspace';

const NONE = new Map<string, string>();

/** CQL tables: `PK` / `CK ↑` / `CK ↓` header markers from the table's key columns. */
export function useTableKeyBadges(object: ObjectRef): Map<string, string> {
  const session = useWorkspace((s) => s.session);
  const [badges, setBadges] = useState(NONE);
  useEffect(() => {
    if (session?.flavor !== 'cql') return;
    let stale = false;
    api
      .keyRoles({ sessionId: session.session_id }, object)
      .then((keys) => !stale && setBadges(keyBadges(keys)))
      .catch(() => undefined);
    return () => {
      stale = true;
    };
  }, [session?.flavor, session?.session_id, object]);
  return badges;
}
