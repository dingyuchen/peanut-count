import * as SQLite from 'expo-sqlite';

let database;

async function open() {
  database ??= (async () => {
    const db = await SQLite.openDatabaseAsync('peanut-count.sqlite3');
    await db.execAsync(`
      PRAGMA journal_mode = WAL;
      CREATE TABLE IF NOT EXISTS statements (
        scope TEXT NOT NULL,
        id TEXT NOT NULL,
        body TEXT NOT NULL,
        pending INTEGER NOT NULL,
        PRIMARY KEY (scope, id)
      );
      CREATE TABLE IF NOT EXISTS preferences (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS cursors (
        scope TEXT PRIMARY KEY,
        after INTEGER NOT NULL
      );
    `);
    return db;
  })().catch((error) => { database = undefined; throw error; });
  return database;
}

function encode({ id, source, currency, rows }) {
  return JSON.stringify({
    id, source, currency,
    rows: rows.map(({ date, description, amount, account }) => ({ date, description, amount, account })),
  });
}

export async function statements(scope) {
  const db = await open();
  const records = await db.getAllAsync('SELECT body FROM statements WHERE scope = ? ORDER BY rowid DESC', scope);
  return records.map(({ body }) => JSON.parse(body));
}

export async function addStatement(scope, statement, sync) {
  const db = await open();
  const result = await db.runAsync(
    'INSERT INTO statements VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING',
    scope, statement.id, encode(statement), sync ? 1 : 0,
  );
  if (result.changes === 0) {
    throw new Error('This PDF is already in this ledger.');
  }
}

export async function pendingStatements(scope) {
  const db = await open();
  const found = await db.getAllAsync('SELECT body FROM statements WHERE scope = ? AND pending = 1 ORDER BY rowid', scope);
  return found.map(({ body }) => JSON.parse(body));
}

export async function markSent(scope, id) {
  const db = await open();
  await db.runAsync('UPDATE statements SET pending = 0 WHERE scope = ? AND id = ?', scope, id);
}

export async function getCursor(scope) {
  const db = await open();
  const row = await db.getFirstAsync('SELECT after FROM cursors WHERE scope = ?', scope);
  return row?.after ?? 0;
}

export async function applyPage(scope, items, after) {
  const db = await open();
  let conflicts = 0;
  await db.withExclusiveTransactionAsync(async (tx) => {
    for (const statement of items) {
      const body = encode(statement);
      const local = await tx.getFirstAsync('SELECT body, pending FROM statements WHERE scope = ? AND id = ?', scope, statement.id);
      if (local && encode(JSON.parse(local.body)) !== body) {
        // The server already accepted this PDF on another device. Preserve both versions.
        await tx.runAsync('UPDATE statements SET pending = 2 WHERE scope = ? AND id = ?', scope, statement.id);
        conflicts++;
      } else {
        await tx.runAsync(
          'INSERT INTO statements VALUES (?, ?, ?, 0) ON CONFLICT(scope, id) DO UPDATE SET pending = 0',
          scope, statement.id, body,
        );
      }
    }
    await tx.runAsync(
      'INSERT INTO cursors VALUES (?, ?) ON CONFLICT(scope) DO UPDATE SET after = max(after, excluded.after)',
      scope, after,
    );
  });
  return conflicts;
}

export async function conflictCount(scope) {
  const db = await open();
  return (await db.getFirstAsync('SELECT count(*) AS total FROM statements WHERE scope = ? AND pending = 2', scope)).total;
}

export async function resolveConflicts(scope) {
  const db = await open();
  await db.withExclusiveTransactionAsync(async (tx) => {
    // Keep the discarded device copy in a separate local-only archive.
    await tx.runAsync('UPDATE statements SET scope = ? WHERE scope = ? AND pending = 2', `archive:${scope}:${Date.now()}`, scope);
    await tx.runAsync('DELETE FROM cursors WHERE scope = ?', scope);
  });
}

export async function loadMode() {
  const db = await open();
  const row = await db.getFirstAsync("SELECT value FROM preferences WHERE key = 'mode'");
  return row?.value;
}

export async function saveMode(mode) {
  const db = await open();
  await db.runAsync(
    "INSERT INTO preferences VALUES ('mode', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    mode,
  );
}
