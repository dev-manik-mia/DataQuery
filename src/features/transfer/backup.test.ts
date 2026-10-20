import { describe, expect, it } from 'vitest';

import { backupFileName, defaultOptions, guessFormat, nameList, toolsFor } from './backup';

describe('backup helpers', () => {
  const now = new Date(2026, 9, 20, 14, 5);

  it('names backup files after the database, time and format', () => {
    expect(backupFileName('app', 'custom', false, now)).toBe('app-2026-10-20-1405.dump');
    expect(backupFileName('app', 'archive', true, now)).toBe('app-2026-10-20-1405.archive.gz');
    expect(backupFileName('app', 'directory', false, now)).toBe('app-2026-10-20-1405');
    expect(backupFileName('/data/shop.sqlite', 'file', false, now)).toBe('shop-2026-10-20-1405.sqlite');
    expect(backupFileName('my db', 'plain', false, now)).toBe('my_db-2026-10-20-1405.sql');
  });

  it('splits name lists on commas and lines', () => {
    expect(nameList(' public.a, sales.*\n\nb ,')).toEqual(['public.a', 'sales.*', 'b']);
  });

  it('picks the tools and defaults per family', () => {
    expect(toolsFor('postgres', 'backup', 'plain')).toEqual(['pg_dump']);
    expect(toolsFor('postgres', 'restore', 'plain')).toEqual(['psql']);
    expect(toolsFor('postgres', 'restore', null)).toEqual(['pg_restore', 'psql']);
    expect(toolsFor('mysql', 'restore', 'plain')).toEqual(['mysql']);
    expect(toolsFor('sqlite', 'backup', 'file')).toEqual([]);
    expect(defaultOptions('postgres', 'custom', 'app', 'sales').schemas).toEqual(['sales']);
    expect(defaultOptions('mysql', 'plain', 'app', null)).toMatchObject({ schemas: [], clean: true, routines: true });
  });

  it('guesses PostgreSQL restore formats from the name', () => {
    expect(guessFormat('postgres', '/b/app.sql')).toBe('plain');
    expect(guessFormat('postgres', '/b/app.dump')).toBe('custom');
    expect(guessFormat('postgres', '/b/folder')).toBeNull();
    expect(guessFormat('mysql', '/b/app.sql')).toBeNull();
  });
});
