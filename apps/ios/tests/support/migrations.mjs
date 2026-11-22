// Node cannot import .sql files the way Metro does, so read the generated migrations directly.
import fs from 'node:fs';
const folder = new URL('../../drizzle/', import.meta.url);
const journal = JSON.parse(fs.readFileSync(new URL('meta/_journal.json', folder), 'utf8'));
const migrations = Object.fromEntries(journal.entries.map((entry) =>
  [`m${String(entry.idx).padStart(4, '0')}`, fs.readFileSync(new URL(`${entry.tag}.sql`, folder), 'utf8')]));
export default { journal, migrations };
