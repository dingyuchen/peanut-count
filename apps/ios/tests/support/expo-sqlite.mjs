// A stand-in for expo-sqlite backed by node:sqlite. It implements only the async API
// the app uses; the sync API throws, so tests fail if a query would block the JS thread.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';

export const defaultDatabaseDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'peanut-ledger-'));

// Every native call resolves on a later macrotask, like a call to a background queue.
const later = (work) => new Promise((resolve, reject) => setImmediate(() => {
  try { resolve(work()); } catch (error) { reject(error); }
}));
const isCipherPragma = (source) => /^\s*PRAGMA (key|cipher_version)\b/i.test(source);
const syncCall = () => { throw new Error('Synchronous expo-sqlite calls block the JS thread.'); };

function statementFor(db, source) {
  let statement = db.prepare(source);
  // Like a native prepared statement, bindings are shared state: rows are read in a
  // later call from whatever was bound last, so overlapping executions would mix.
  let bound = [];
  return {
    executeAsync: (params) => later(() => {
      const result = statement.run(...params);
      return { changes: Number(result.changes), lastInsertRowId: Number(result.lastInsertRowid) };
    }),
    executeForRawResultAsync: (params) => later(() => {
      bound = params;
      const read = () => { statement.setReturnArrays(true); return statement.all(...bound); };
      return { getAllAsync: () => later(read), getFirstAsync: () => later(() => read()[0] ?? null) };
    }),
    finalizeAsync: () => later(() => { statement = null; }),
  };
}

export async function openDatabaseAsync(name) {
  const db = new DatabaseSync(path.join(defaultDatabaseDirectory, name));
  return {
    execAsync: (source) => later(() => { if (!isCipherPragma(source)) db.exec(source); }),
    getFirstAsync: (source, ...params) => later(() => (isCipherPragma(source)
      ? { cipher_version: 'test' } : db.prepare(source).get(...params) ?? null)),
    runAsync: (source, ...params) => later(() => db.prepare(source).run(...params)),
    prepareAsync: (source) => later(() => statementFor(db, source)),
    closeAsync: () => later(() => db.close()),
    withExclusiveTransactionAsync: () => { throw new Error('A second connection would not have the SQLCipher key.'); },
    prepareSync: syncCall, execSync: syncCall, runSync: syncCall, getAllSync: syncCall, getFirstSync: syncCall,
  };
}

// Database files whose deletion should fail, as when iOS cannot remove a file, and
// every deletion attempted, in order.
export const failDeletes = new Set();
export const deletions = [];

// Same messages as expo-sqlite's native deleteDatabase.
export async function deleteDatabaseAsync(name) {
  const file = path.join(defaultDatabaseDirectory, name);
  await later(() => {
    deletions.push(name);
    if (failDeletes.has(name)) throw new Error(`Unable to delete the database file for ${file} database`);
    if (!fs.existsSync(file)) throw new Error(`Database ${file} not found`);
    fs.rmSync(file);
  });
}
