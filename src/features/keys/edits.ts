import type { DbError } from '@/bindings/DbError';
import type { KeyEdit } from '@/bindings/KeyEdit';
import type { KeyRef } from '@/bindings/KeyRef';
import type { KeyType } from '@/bindings/KeyType';
import type { RString } from '@/bindings/RString';
import { toAppError } from '@/lib/errors';
import { api } from '@/lib/ipc';
import { guarded } from '@/stores/ui';

/** Text typed by the user, sent as exact UTF-8. */
export const plain = (text: string): RString => ({ text, binary: false });

/**
 * Sends one key change through the backend's confirmation rules. Returns
 * `null` on success (or when the user declined), else the error to show.
 */
export async function writeKey(sessionId: string, key: KeyRef, edit: KeyEdit, create = false): Promise<{ error: DbError | null; done: boolean }> {
  try {
    const result = await guarded((c) => api.keysWrite({ sessionId }, { key, edit, create }, c));
    return { error: null, done: result !== null };
  } catch (e) {
    return { error: toAppError(e).info, done: false };
  }
}

/** The first change that creates a key of `type`, from the New-key form. */
export function firstEdit(type: KeyType, values: Record<string, string>): KeyEdit | string {
  const v = (name: string) => values[name] ?? '';
  switch (type) {
    case 'string':
      return { op: 'set_string', value: plain(v('value')) };
    case 'hash':
      return v('field') ? { op: 'hash_set', field: plain(v('field')), value: plain(v('value')) } : 'Enter the first field.';
    case 'list':
      return { op: 'list_push', value: plain(v('value')), head: false };
    case 'set':
      return { op: 'set_add', member: plain(v('value')) };
    case 'zset':
      return Number.isNaN(Number(v('score'))) && !/^[+-]?inf$/i.test(v('score').trim()) ? 'The score must be a number.' : { op: 'zset_add', member: plain(v('value')), score: v('score').trim() || '0' };
    case 'stream':
      return v('field') ? { op: 'stream_add', fields: [[plain(v('field')), plain(v('value'))]] } : 'Enter the first field.';
    case 'other':
      return 'Module types are created with their own commands in the console.';
  }
}
