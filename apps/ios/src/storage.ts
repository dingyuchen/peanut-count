import { DrizzleQueryError, and, asc, count, desc, eq, inArray, sql } from 'drizzle-orm';
import * as Crypto from 'expo-crypto';
import * as SecureStore from 'expo-secure-store';
import * as SQLite from 'expo-sqlite';
import migrations from '../drizzle/migrations';
import { LedgerClient } from './db/client';
import { CONFLICT, PENDING, SENT, cursors, preferences, statements as statementTable, type Statement } from './db/schema';

export type { Statement, StatementRow } from './db/schema';
export type LedgerMode = 'local' | 'synced';

// The ledger is always stored with SQLCipher (see the expo-sqlite plugin in app.json).
// A random 256-bit key lives in the iOS Keychain; the file on disk is never plaintext.
const DATABASE = 'ledger.sqlite3';
const LEGACY_DATABASE = 'peanut-count.sqlite3';
const KEY_ITEM = 'peanut.ledger.key';
const LEGACY_DONE_ITEM = 'peanut.ledger.legacy-migrated';
// Set while an erase is under way, so an interrupted erase is finished before the next open.
const ERASING_ITEM = 'peanut.ledger.erasing';
const keychain: SecureStore.SecureStoreOptions = { keychainAccessible: SecureStore.AFTER_FIRST_UNLOCK };

export class LockedLedgerError extends Error {}

let database: Promise<LedgerClient> | undefined;
let erasing: Promise<void> | undefined;

// expo-sqlite reports a missing file and a failed deletion with the same error code,
// so only the message tells them apart.
const isMissingFile = (error: unknown): boolean => error instanceof Error && /\bnot found\b/i.test(error.message);

// Deletes a database and its WAL files. A file that is already gone counts as deleted;
// any other failure is thrown. The main file goes first: until it is gone, its WAL may
// hold committed changes that are not in it yet. If a WAL file then survives, it must
// not meet a new database, so callers finish the deletion before opening again.
async function removeDatabaseFiles(name: string): Promise<void> {
  for (const file of [name, `${name}-wal`, `${name}-shm`]) {
    try {
      await SQLite.deleteDatabaseAsync(file);
    } catch (error) {
      if (!isMissingFile(error)) throw error;
    }
  }
}

async function databaseKey(): Promise<string> {
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

async function requireCipher(db: SQLite.SQLiteDatabase): Promise<void> {
  const row = await db.getFirstAsync<{ cipher_version?: string }>('PRAGMA cipher_version');
  if (!row?.cipher_version) {
    // Refuse to fall back to plaintext if the app was built without SQLCipher.
    throw new Error('This build cannot encrypt the ledger. Rebuild the app with SQLCipher enabled.');
  }
}

// Builds before encryption wrote a plaintext file. Copy it into the encrypted
// database once, then delete it. A retry after an interruption starts the copy over.
async function migrateLegacy(key: string): Promise<void> {
  if (await SecureStore.getItemAsync(LEGACY_DONE_ITEM, keychain)) return;
  const legacy = await SQLite.openDatabaseAsync(LEGACY_DATABASE, { useNewConnection: true });
  try {
    await requireCipher(legacy);
    const found = await legacy.getFirstAsync<{ tables: number }>("SELECT count(*) AS tables FROM sqlite_master WHERE type = 'table'");
    if (found && found.tables > 0) {
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

// Deletes the ledger files, then the key: a new key could never unlock a surviving
// ledger. The marker stays until both are gone, so an erase that stops part way is
// finished by the next open instead of leaving a stray WAL for a new database.
async function eraseFiles(): Promise<void> {
  await SecureStore.setItemAsync(ERASING_ITEM, '1', keychain);
  await removeDatabaseFiles(DATABASE);
  await SecureStore.deleteItemAsync(KEY_ITEM, keychain);
  await SecureStore.deleteItemAsync(ERASING_ITEM, keychain);
}

async function openEncrypted(): Promise<LedgerClient> {
  if (await SecureStore.getItemAsync(ERASING_ITEM, keychain)) await eraseFiles();
  const key = await databaseKey();
  await migrateLegacy(key);
  // A connection of its own: expo-sqlite would otherwise share its cached native
  // connection for this path and, in development builds, register it with DevTools,
  // whose queries would bypass LedgerClient's queue and could land inside a transaction.
  const client = await SQLite.openDatabaseAsync(DATABASE, { useNewConnection: true });
  try {
    // PRAGMA key must run before anything else touches the file.
    await client.execAsync(`PRAGMA key = "x'${key}'"`);
    await requireCipher(client);
    try {
      await client.getFirstAsync('SELECT count(*) FROM sqlite_master');
    } catch {
      throw new LockedLedgerError('This ledger cannot be unlocked with the key in this device\'s Keychain.');
    }
    // Migrations run inside a transaction, where the journal mode cannot change.
    await client.execAsync('PRAGMA journal_mode = WAL');
  } catch (error) {
    await client.closeAsync().catch(() => {});
    throw error;
  }
  const ledger = new LedgerClient(client);
  try {
    await ledger.migrate(migrations);
  } catch (error) {
    await ledger.close().catch(() => {});
    throw error;
  }
  return ledger;
}

function open(): Promise<LedgerClient> {
  // An open that starts during an erase waits for it, so it never reopens the old
  // file or reads a key that is about to be deleted.
  if (database) return database;
  const erased = erasing?.catch(() => {}) ?? Promise.resolve();
  const opening = erased.then(openEncrypted);
  database = opening;
  // Forget a failed open so the next call retries, unless an erase has already
  // replaced it: clearing a newer open would let two opens race to create the file.
  opening.catch(() => { if (database === opening) database = undefined; });
  return opening;
}

// Erase a ledger that can no longer be decrypted (for example, after its Keychain
// entry was lost) and start over with a new key. Overlapping calls share one erase.
export function eraseLocalLedger(): Promise<void> {
  erasing ??= (async () => {
    const previous = database;
    database = undefined;
    const ledger = await previous?.catch(() => null);
    // Queries already queued on the old connection finish before it closes. If it
    // cannot close, the deletion below fails rather than deleting an open file.
    await ledger?.close().catch(() => {});
    await eraseFiles();
  })().finally(() => { erasing = undefined; });
  return erasing;
}

// The stored form of a statement: only the fields the ledger keeps, in a fixed order,
// so two copies of the same import compare equal as JSON.
function encode({ id, source, currency, rows }: Statement): Statement {
  return {
    id, source, currency,
    rows: rows.map(({ date, description, amount, account }) => ({ date, description, amount, account })),
  };
}

const sameStatement = (a: Statement, b: Statement): boolean => JSON.stringify(encode(a)) === JSON.stringify(encode(b));
const inScope = (scope: string) => eq(statementTable.scope, scope);
// Statements have no timestamp column; rowid order is import order.
const rowid = sql`rowid`;

// Drizzle wraps driver errors in a message with the full SQL and its bound values,
// which include ledger contents. The app shows error messages on screen, so pass on
// the driver's own error instead.
function driverError(error: unknown): unknown {
  return error instanceof DrizzleQueryError && error.cause instanceof Error ? error.cause : error;
}

async function withLedger<T>(work: (ledger: LedgerClient) => Promise<T>): Promise<T> {
  const ledger = await open();
  try {
    return await work(ledger);
  } catch (error) {
    throw driverError(error);
  }
}

export function statements(scope: string): Promise<Statement[]> {
  return withLedger(async ({ db }) => {
    const records = await db.select({ body: statementTable.body }).from(statementTable)
      .where(inScope(scope)).orderBy(desc(rowid)).all();
    return records.map(({ body }) => body);
  });
}

export function addStatement(scope: string, statement: Statement, sync: boolean): Promise<void> {
  return withLedger(async ({ db }) => {
    const inserted = await db.insert(statementTable)
      .values({ scope, id: statement.id, body: encode(statement), pending: sync ? PENDING : SENT })
      .onConflictDoNothing().returning({ id: statementTable.id }).all();
    if (inserted.length === 0) {
      throw new Error('This PDF is already in this ledger.');
    }
  });
}

export function pendingStatements(scope: string): Promise<Statement[]> {
  return withLedger(async ({ db }) => {
    const found = await db.select({ body: statementTable.body }).from(statementTable)
      .where(and(inScope(scope), eq(statementTable.pending, PENDING))).orderBy(asc(rowid)).all();
    return found.map(({ body }) => body);
  });
}

export function markSent(scope: string, id: string): Promise<void> {
  return withLedger(async ({ db }) => {
    await db.update(statementTable).set({ pending: SENT })
      .where(and(inScope(scope), eq(statementTable.id, id))).run();
  });
}

export function getCursor(scope: string): Promise<number> {
  return withLedger(async ({ db }) => {
    const row = await db.select({ after: cursors.after }).from(cursors).where(eq(cursors.scope, scope)).get();
    return row?.after ?? 0;
  });
}

// Statements are looked up and written a chunk at a time. Each statement row binds
// four values, so a chunk stays far below SQLite's bound-parameter limit.
const PAGE_CHUNK = 100;

export function applyPage(scope: string, items: Statement[], after: number): Promise<number> {
  return withLedger(async (ledger) => {
    return ledger.transaction(async (tx) => {
      let conflicts = 0;
      for (let start = 0; start < items.length; start += PAGE_CHUNK) {
        const chunk = items.slice(start, start + PAGE_CHUNK);
        const found = await tx.select({ id: statementTable.id, body: statementTable.body }).from(statementTable)
          .where(and(inScope(scope), inArray(statementTable.id, chunk.map(({ id }) => id)))).all();
        const local = new Map(found.map(({ id, body }) => [id, body]));
        // Decide each row in page order, as one-at-a-time processing would: a new ID is
        // inserted on first sight, and a repeated ID ends with its last outcome.
        const added: Statement[] = [];
        const outcome = new Map<string, typeof SENT | typeof CONFLICT>();
        for (const statement of chunk) {
          const existing = local.get(statement.id);
          if (existing && !sameStatement(existing, statement)) {
            // The server already accepted this PDF on another device. Preserve both versions.
            outcome.set(statement.id, CONFLICT);
            conflicts++;
          } else {
            outcome.set(statement.id, SENT);
            if (!existing) {
              local.set(statement.id, statement);
              added.push(statement);
            }
          }
        }
        if (added.length) {
          await tx.insert(statementTable)
            .values(added.map((statement): typeof statementTable.$inferInsert => ({ scope, id: statement.id, body: encode(statement), pending: SENT })))
            .run();
        }
        // Rows added above are already SENT, so only rows that existed before need it.
        const existed = new Set(found.map(({ id }) => id));
        for (const state of [SENT, CONFLICT] as const) {
          const ids = [...outcome].filter(([id, value]) => value === state && (state === CONFLICT || existed.has(id))).map(([id]) => id);
          if (ids.length) {
            await tx.update(statementTable).set({ pending: state })
              .where(and(inScope(scope), inArray(statementTable.id, ids))).run();
          }
        }
      }
      await tx.insert(cursors).values({ scope, after })
        .onConflictDoUpdate({ target: cursors.scope, set: { after: sql`max(${cursors.after}, excluded.${sql.identifier('after')})` } }).run();
      return conflicts;
    });
  });
}

export function conflictCount(scope: string): Promise<number> {
  return withLedger(async ({ db }) => {
    const row = await db.select({ total: count() }).from(statementTable)
      .where(and(inScope(scope), eq(statementTable.pending, CONFLICT))).get();
    return row?.total ?? 0;
  });
}

export function resolveConflicts(scope: string): Promise<void> {
  return withLedger(async (ledger) => {
    await ledger.transaction(async (tx) => {
      // Keep the discarded device copy in a separate local-only archive.
      await tx.update(statementTable).set({ scope: `archive:${scope}:${Date.now()}` })
        .where(and(inScope(scope), eq(statementTable.pending, CONFLICT))).run();
      await tx.delete(cursors).where(eq(cursors.scope, scope)).run();
    });
  });
}

export function loadMode(): Promise<string | undefined> {
  return withLedger(async ({ db }) => {
    const row = await db.select({ value: preferences.value }).from(preferences).where(eq(preferences.key, 'mode')).get();
    return row?.value;
  });
}

export function saveMode(mode: LedgerMode): Promise<void> {
  return withLedger(async ({ db }) => {
    await db.insert(preferences).values({ key: 'mode', value: mode })
      .onConflictDoUpdate({ target: preferences.key, set: { value: mode } }).run();
  });
}
