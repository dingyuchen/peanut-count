import * as Crypto from 'expo-crypto';
import * as SecureStore from 'expo-secure-store';
import * as SQLite from 'expo-sqlite';

// The ledger is always stored with SQLCipher (see the expo-sqlite plugin in app.json).
// A random 256-bit key lives in the iOS Keychain; the file on disk is never plaintext.
const DATABASE = 'ledger.sqlite3';
const LEGACY_DATABASE = 'peanut-count.sqlite3';
const KEY_ITEM = 'peanut.ledger.key';
const LEGACY_DONE_ITEM = 'peanut.ledger.legacy-migrated';
const keychain = { keychainAccessible: SecureStore.AFTER_FIRST_UNLOCK };

export class LockedLedgerError extends Error {}

let database;

async function removeDatabaseFiles(name) {
  for (const file of [name, `${name}-wal`, `${name}-shm`]) {
    await SQLite.deleteDatabaseAsync(file).catch(() => {});
  }
}

async function databaseKey() {
  const stored = await SecureStore.getItemAsync(KEY_ITEM, keychain);
  if (stored !== null) {
    if (!/^[0-9a-f]{64}$/.test(stored)) throw new LockedLedgerError('The ledger key in the Keychain is damaged.');
    return stored;
  }
  const bytes = await Crypto.getRandomBytesAsync(32);
  const key = Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
  await SecureStore.setItemAsync(KEY_ITEM, key, keychain);
  return key;
}

async function requireCipher(db) {
  const row = await db.getFirstAsync('PRAGMA cipher_version');
  if (!row?.cipher_version) {
    // Refuse to fall back to plaintext if the app was built without SQLCipher.
    throw new Error('This build cannot encrypt the ledger. Rebuild the app with SQLCipher enabled.');
  }
}

// Builds before encryption wrote a plaintext file. Copy it into the encrypted
// database once, then delete it. A retry after an interruption starts the copy over.
async function migrateLegacy(key) {
  if (await SecureStore.getItemAsync(LEGACY_DONE_ITEM, keychain)) return;
  const legacy = await SQLite.openDatabaseAsync(LEGACY_DATABASE, { useNewConnection: true });
  try {
    await requireCipher(legacy);
    const { tables } = await legacy.getFirstAsync("SELECT count(*) AS tables FROM sqlite_master WHERE type = 'table'");
    if (tables > 0) {
      await removeDatabaseFiles(DATABASE);
      await legacy.execAsync('PRAGMA journal_mode = DELETE');
      await legacy.runAsync('ATTACH DATABASE ? AS encrypted KEY ?', `${SQLite.defaultDatabaseDirectory}/${DATABASE}`, `x'${key}'`);
      await legacy.getFirstAsync("SELECT sqlcipher_export('encrypted')");
      await legacy.execAsync('DETACH DATABASE encrypted');
    }
  } finally {
    await legacy.closeAsync();
  }
  await removeDatabaseFiles(LEGACY_DATABASE);
  await SecureStore.setItemAsync(LEGACY_DONE_ITEM, '1', keychain);
}

async function openEncrypted() {
  const key = await databaseKey();
  await migrateLegacy(key);
  const db = await SQLite.openDatabaseAsync(DATABASE);
  try {
    // PRAGMA key must run before anything else touches the file.
    await db.execAsync(`PRAGMA key = "x'${key}'"`);
    await requireCipher(db);
    try {
      await db.getFirstAsync('SELECT count(*) FROM sqlite_master');
    } catch {
      throw new LockedLedgerError('This ledger cannot be unlocked with the key in this device\'s Keychain.');
    }
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
  } catch (error) {
    await db.closeAsync().catch(() => {});
    throw error;
  }
}

async function open() {
  database ??= openEncrypted().catch((error) => { database = undefined; throw error; });
  return database;
}

// Erase a ledger that can no longer be decrypted (for example, after its Keychain
// entry was lost) and start over with a new key.
export async function eraseLocalLedger() {
  const db = await database?.catch(() => null);
  database = undefined;
  await db?.closeAsync().catch(() => {});
  await removeDatabaseFiles(DATABASE);
  await SecureStore.deleteItemAsync(KEY_ITEM, keychain);
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
