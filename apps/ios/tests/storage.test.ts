import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { beforeEach, test } from 'node:test';
import { eq } from 'drizzle-orm';
import * as SecureStore from 'expo-secure-store';
import * as SQLite from 'expo-sqlite';
import { defaultDatabaseDirectory, openDatabaseAsync } from 'expo-sqlite';
import migrations from '../drizzle/migrations';
import { LedgerClient, type LedgerDb } from '../src/db/client.ts';
import { preferences } from '../src/db/schema.ts';
import * as storage from '../src/storage.ts';
import type { Statement } from '../src/storage.ts';

const LEDGER = path.join(defaultDatabaseDirectory, 'ledger.sqlite3');

const statement = (id: string, account = 'Expenses:Dining'): Statement => ({
  id, source: 'Assets:Checking', currency: 'USD',
  rows: [{ date: '2026-11-02', description: 'Lunch', amount: '-12.00', account }],
});

function inspect<T>(query: string): T {
  const db = new DatabaseSync(LEDGER);
  try { return db.prepare(query).get() as T; } finally { db.close(); }
}

beforeEach(() => storage.eraseLocalLedger());

test('stores, deduplicates, and lists statements newest first', async () => {
  assert.equal(await storage.loadMode(), undefined);
  await storage.saveMode('local');
  await storage.saveMode('synced');
  assert.equal(await storage.loadMode(), 'synced');

  await storage.addStatement('device', statement('a'), false);
  // Fields outside the stored shape are dropped, so copies compare equal as JSON.
  await storage.addStatement('device', { ...statement('b'), extra: 'dropped' } as Statement, false);
  await assert.rejects(storage.addStatement('device', statement('a'), false), /already in this ledger/);
  assert.deepEqual((await storage.statements('device')).map(({ id }) => id), ['b', 'a']);
  assert.deepEqual(await storage.statements('elsewhere'), []);
  assert.equal(inspect<{ body: string }>("SELECT body FROM statements WHERE id = 'b'").body, JSON.stringify(statement('b')));
});

test('tracks uploads, conflicts, and the sync cursor', async () => {
  await storage.addStatement('s', statement('p1'), true);
  await storage.addStatement('s', statement('p2'), true);
  assert.deepEqual((await storage.pendingStatements('s')).map(({ id }) => id), ['p1', 'p2']);
  await storage.markSent('s', 'p1');
  assert.deepEqual((await storage.pendingStatements('s')).map(({ id }) => id), ['p2']);

  assert.equal(await storage.getCursor('s'), 0);
  // p1 matches the server, p2 differs from it, and n1 is new from another device.
  assert.equal(await storage.applyPage('s', [statement('p1'), statement('p2', 'Expenses:Other'), statement('n1')], 10), 1);
  assert.equal(await storage.conflictCount('s'), 1);
  assert.equal(await storage.getCursor('s'), 10);
  await storage.applyPage('s', [], 4);
  assert.equal(await storage.getCursor('s'), 10, 'the cursor never moves backwards');
  assert.deepEqual(await storage.pendingStatements('s'), []);

  await storage.resolveConflicts('s');
  assert.equal(await storage.conflictCount('s'), 0);
  assert.equal(await storage.getCursor('s'), 0);
  assert.deepEqual((await storage.statements('s')).map(({ id }) => id).sort(), ['n1', 'p1']);
  assert.equal(inspect<{ n: number }>("SELECT count(*) AS n FROM statements WHERE scope LIKE 'archive:s:%' AND pending = 2").n, 1);
});

test('applies a sync page in order, including repeated IDs, as the row-by-row version did', async () => {
  await storage.addStatement('s', statement('local'), true);
  const page = [
    statement('n1'), statement('local', 'Expenses:Other'), statement('n2'),
    statement('n1'), statement('n2', 'Expenses:Other'),
  ];
  // 'local' differs from this device's copy; the second 'n2' differs from the first one in this page.
  assert.equal(await storage.applyPage('s', page, 3), 2);
  assert.deepEqual((await storage.statements('s')).map(({ id }) => id), ['n2', 'n1', 'local']);
  assert.equal(await storage.conflictCount('s'), 2);
  assert.deepEqual((await storage.statements('s')).find(({ id }) => id === 'n2'), statement('n2'));

  // A repeated ID ends with its last outcome: a differing copy, then this device's copy.
  await storage.resolveConflicts('s');
  await storage.addStatement('r', statement('mine'), false);
  assert.equal(await storage.applyPage('r', [statement('mine', 'Expenses:Other'), statement('mine')], 4), 1);
  assert.equal(await storage.conflictCount('r'), 0);

  // Pages larger than one lookup chunk.
  const many = Array.from({ length: 250 }, (_, index) => statement(`m${index}`));
  assert.equal(await storage.applyPage('t', many, 9), 0);
  assert.equal((await storage.statements('t')).length, 250);
  assert.equal((await storage.statements('t'))[0].id, 'm249');
});

test('adopts a ledger created before Drizzle', async () => {
  const old = statement('old');
  const db = new DatabaseSync(LEDGER);
  db.exec(`
    CREATE TABLE statements (scope TEXT NOT NULL, id TEXT NOT NULL, body TEXT NOT NULL, pending INTEGER NOT NULL, PRIMARY KEY (scope, id));
    CREATE TABLE preferences (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE cursors (scope TEXT PRIMARY KEY, after INTEGER NOT NULL);
  `);
  db.prepare('INSERT INTO statements VALUES (?, ?, ?, ?)').run('device', 'old', JSON.stringify(old), 0);
  db.prepare("INSERT INTO preferences VALUES ('mode', 'synced')").run();
  db.prepare("INSERT INTO cursors VALUES ('s', 7)").run();
  db.close();

  assert.equal(await storage.loadMode(), 'synced');
  assert.deepEqual(await storage.statements('device'), [old]);
  assert.equal(await storage.getCursor('s'), 7);
  await storage.addStatement('device', statement('new'), false);
  assert.deepEqual((await storage.statements('device')).map(({ id }) => id), ['new', 'old']);
  assert.equal(inspect<{ n: number }>('SELECT count(*) AS n FROM __drizzle_migrations').n, 1);
});

test('an erase waits for queued work and delays new opens until it finishes', async () => {
  await storage.saveMode('synced');
  const [before, , after] = await Promise.all([storage.loadMode(), storage.eraseLocalLedger(), storage.loadMode()]);
  assert.equal(before, 'synced');
  assert.equal(after, undefined);
  await Promise.all([storage.eraseLocalLedger(), storage.eraseLocalLedger()]);
  assert.equal(await storage.loadMode(), undefined);
});

test('a failed open during an erase does not start a second open', async () => {
  // A damaged Keychain entry makes the pending open fail with LockedLedgerError.
  await SecureStore.setItemAsync('peanut.ledger.key', 'damaged');
  const keyWrites = () => (SecureStore as unknown as { writes: string[] }).writes.filter((key) => key === 'peanut.ledger.key').length;
  const before = keyWrites();
  const failing = storage.loadMode();
  const erase = storage.eraseLocalLedger();
  const replacement = storage.loadMode();
  await assert.rejects(failing, storage.LockedLedgerError);
  // The failed open must not clear the replacement scheduled during the erase.
  const again = storage.loadMode();
  await erase;
  assert.deepEqual(await Promise.all([replacement, again]), [undefined, undefined]);
  assert.equal(keyWrites() - before, 1, 'exactly one new key is created');
});

const sqliteDouble = SQLite as unknown as { failDeletes: Set<string>; deletions: string[] };

async function eraseFailing(file: string): Promise<string[]> {
  sqliteDouble.deletions.length = 0;
  sqliteDouble.failDeletes.add(file);
  try {
    await assert.rejects(storage.eraseLocalLedger(), /Unable to delete/);
  } finally {
    sqliteDouble.failDeletes.clear();
  }
  return [...sqliteDouble.deletions];
}

test('a failed erase keeps the WAL with the ledger, and the next open finishes the erase', async () => {
  await storage.saveMode('synced');
  const key = await SecureStore.getItemAsync('peanut.ledger.key');
  // Committed changes may still be in the WAL, so it is not touched until the main file is gone.
  assert.deepEqual(await eraseFailing('ledger.sqlite3'), ['ledger.sqlite3']);
  assert.equal(fs.existsSync(LEDGER), true);
  assert.equal(await SecureStore.getItemAsync('peanut.ledger.key'), key);
  // The user asked to erase, so the next open completes it before opening anything.
  assert.equal(await storage.loadMode(), undefined);
  assert.notEqual(await SecureStore.getItemAsync('peanut.ledger.key'), key);
  assert.equal(await SecureStore.getItemAsync('peanut.ledger.erasing'), null);
});

test('a WAL left by an interrupted erase is deleted before a new ledger is created', async () => {
  await storage.saveMode('synced');
  assert.deepEqual(await eraseFailing('ledger.sqlite3-wal'), ['ledger.sqlite3', 'ledger.sqlite3-wal']);
  assert.equal(fs.existsSync(LEDGER), false);
  sqliteDouble.deletions.length = 0;
  assert.equal(await storage.loadMode(), undefined);
  assert.deepEqual(sqliteDouble.deletions.slice(0, 3), ['ledger.sqlite3', 'ledger.sqlite3-wal', 'ledger.sqlite3-shm']);
  assert.equal(await SecureStore.getItemAsync('peanut.ledger.erasing'), null);
});

test('concurrent calls all complete', async () => {
  const ids = Array.from({ length: 20 }, (_, index) => `c${index}`);
  await Promise.all(ids.map((id) => storage.addStatement('device', statement(id), false)));
  const [listed, count] = await Promise.all([storage.statements('device'), storage.conflictCount('device')]);
  assert.deepEqual(listed.map(({ id }) => id), [...ids].reverse());
  assert.equal(count, 0);
});

test('a transaction holds the connection until it ends', async () => {
  const client = new LedgerClient(await openDatabaseAsync('client.sqlite3'));
  try {
    await client.migrate(migrations);
    await client.migrate(migrations);
    let release!: () => void;
    const paused = new Promise<void>((resolve) => { release = resolve; });
    const failing = client.transaction(async (tx) => {
      await tx.insert(preferences).values({ key: 'inside', value: '1' }).run();
      await paused;
      throw new Error('abort');
    });
    // Queued while the transaction is open. It must not join the transaction and be rolled back with it.
    const outside = client.db.insert(preferences).values({ key: 'outside', value: '1' }).run();
    release();
    await assert.rejects(failing, /abort/);
    await outside;
    const keys = await client.db.select({ key: preferences.key }).from(preferences).all();
    assert.deepEqual(keys.map(({ key }) => key), ['outside']);
    assert.equal(await client.db.select().from(preferences).where(eq(preferences.key, 'inside')).get(), undefined);
    // Concurrent reads inside a transaction share cached statements without mixing rows.
    await client.db.insert(preferences).values([{ key: 'a', value: '1' }, { key: 'b', value: '2' }]).run();
    const [a, b] = await client.transaction((tx) => Promise.all(['a', 'b'].map((key) =>
      tx.select().from(preferences).where(eq(preferences.key, key)).all())));
    assert.deepEqual([a, b], [[{ key: 'a', value: '1' }], [{ key: 'b', value: '2' }]]);
    // A rejected Promise.all must not leave sibling writes to run after ROLLBACK.
    await assert.rejects(client.transaction((tx) => Promise.all([
      tx.insert(preferences).values({ key: 'a', value: 'duplicate' }).run(),
      ...['x', 'y', 'z'].map((key) => tx.insert(preferences).values({ key, value: '1' }).run()),
    ])), (error: Error) => /UNIQUE/.test(String(error.cause)));
    const after = await client.db.select({ key: preferences.key }).from(preferences).all();
    assert.deepEqual(after.map(({ key }) => key).sort(), ['a', 'b', 'outside']);
    // A query on a transaction that has ended is refused.
    let leaked!: LedgerDb;
    await client.transaction(async (tx) => { leaked = tx; });
    await assert.rejects(leaked.select().from(preferences).all(), (error: Error) => /already ended/.test(String(error.cause)));
    // Drizzle's queue-bypassing transaction and batch helpers are not available.
    assert.throws(() => (client.db as unknown as { transaction(): void }).transaction(), /LedgerClient.transaction/);
  } finally {
    await client.close();
    fs.rmSync(path.join(defaultDatabaseDirectory, 'client.sqlite3'), { force: true });
  }
});
