import { en, type MessageKey } from './en';

/** Available string tables by language code. Add a language by adding a table with the same keys. */
const catalogs: Record<string, Partial<Record<MessageKey, string>>> = { en };

let current = 'en';

export function setLanguage(code: string) {
  current = catalogs[code] ? code : 'en';
}

export const languages = Object.keys(catalogs);

/** The string for `key` in the current language (falling back to English), with `{name}` filled in. */
export function t(key: MessageKey, vars?: Record<string, string | number>): string {
  const text = catalogs[current]?.[key] ?? en[key];
  return vars ? text.replace(/\{(\w+)\}/g, (_, name: string) => String(vars[name] ?? `{${name}}`)) : text;
}

export type { MessageKey };
