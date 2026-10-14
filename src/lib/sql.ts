import type { StatementInfo } from '@/bindings/StatementInfo';

/**
 * The statement to run for a cursor position: the one containing it, else the
 * nearest one before it (cursor on a blank line after a statement), else the next.
 */
export function statementAt(statements: StatementInfo[], cursor: number): StatementInfo | undefined {
  let previous: StatementInfo | undefined;
  for (const s of statements) {
    if (cursor >= s.from && cursor <= s.to) return s;
    if (s.to < cursor) previous = s;
    else return previous ?? s;
  }
  return previous;
}

/** A short title for a tab or history entry. */
export function sqlTitle(sql: string, max = 40): string {
  const flat = sql
    .replace(/--[^\n]*/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();
  return flat.length > max ? `${flat.slice(0, max)}…` : flat || 'Empty query';
}
