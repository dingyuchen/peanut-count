import { drizzle, type AsyncRemoteCallback, type SqliteRemoteDatabase } from 'drizzle-orm/sqlite-proxy';
import type { SQLiteDatabase, SQLiteStatement } from 'expo-sqlite';
import * as schema from './schema';

type ProxyDb = SqliteRemoteDatabase<typeof schema>;

// Drizzle's own transaction() would queue BEGIN, each statement, and COMMIT as
// separate turns, so other queries could run inside the transaction and be rolled
// back with it. batch() needs a batch callback this client does not provide. Both
// are removed; use LedgerClient.transaction() instead.
export type LedgerDb = Omit<ProxyDb, 'transaction' | 'batch'>;

function withoutTransactions(db: ProxyDb): LedgerDb {
  const unsupported = (): never => {
    throw new Error('Use LedgerClient.transaction() for transactions on the ledger.');
  };
  return Object.assign(db, { transaction: unsupported, batch: unsupported });
}

export interface Migrations {
  journal: { entries: readonly { idx: number; when: number; tag: string }[] };
  migrations: Record<string, string>;
}

// Prepared statements are reused across calls. The app issues a small, fixed set of
// queries, so this bound is never reached in practice; it only caps memory.
const STATEMENT_CACHE_SIZE = 32;

// Runs tasks one at a time, in call order. A failed task does not block later ones.
class SerialQueue {
  private tail: Promise<unknown> = Promise.resolve();

  run<T>(task: () => Promise<T>): Promise<T> {
    const result = this.tail.then(task);
    this.tail = result.catch(() => {});
    return result;
  }

  // Resolves once every task queued so far has settled.
  async idle(): Promise<void> {
    let tail;
    do {
      tail = this.tail;
      await tail;
    } while (tail !== this.tail);
  }
}

// Drizzle on top of expo-sqlite's async API. Every statement runs on expo-sqlite's
// native background queue, so queries never block the JavaScript thread.
//
// All work shares the single keyed SQLCipher connection, which the client must own
// outright (see openEncrypted in storage.ts). (expo-sqlite's
// withExclusiveTransactionAsync opens a second connection, which would not have
// the key.) A serial queue keeps statements from interleaving: plain queries each
// take one turn, and a transaction holds the queue from BEGIN to COMMIT.
//
// Why sqlite-proxy: drizzle-orm 0.45's own expo-sqlite driver uses expo-sqlite's
// synchronous API, which blocks the JS thread (drizzle-orm issue #5240). Drizzle
// 1.0 adds an async driver, `drizzle-orm/expo-sqlite/async` (from 1.0.0-rc.5).
// When 1.0 is stable, switch to it and drop the proxy callback, `migrate` here and
// the DrizzleQueryError unwrapping in storage.ts (1.0 leaves parameters out of
// error messages unless `paramsInErrors` is set). Keep the queue and transaction
// handling: that driver runs BEGIN, each statement and COMMIT as separate calls on
// the shared connection, so other queries can land inside a transaction, and a
// rejected Promise.all can leave writes to run after ROLLBACK. It also prepares
// and finalizes a statement for every query instead of reusing one.
export class LedgerClient {
  readonly db: LedgerDb;
  private readonly queue = new SerialQueue();
  private readonly statements = new Map<string, SQLiteStatement>();
  private readonly connection: SQLiteDatabase;
  private closed = false;

  constructor(connection: SQLiteDatabase) {
    this.connection = connection;
    this.db = withoutTransactions(drizzle((query, params, method) => this.queue.run(() => this.execute(query, params, method)), { schema }));
  }

  // Runs `work` in an IMMEDIATE transaction. Queries inside must use the `tx`
  // argument; using `db` there would wait for the transaction and never finish.
  transaction<T>(work: (tx: LedgerDb) => Promise<T>): Promise<T> {
    return this.queue.run(async () => {
      // Queries inside the transaction also run one at a time, so concurrent ones
      // (for example under Promise.all) never share a cached statement mid-read.
      const inner = new SerialQueue();
      let ended = false;
      const tx = withoutTransactions(drizzle((query, params, method) => inner.run(() => {
        if (ended) throw new Error('This transaction has already ended.');
        return this.execute(query, params, method);
      }), { schema }));
      // Once `work` settles, no further query may start, and any query still running
      // finishes before COMMIT or ROLLBACK. Otherwise a query left over from a rejected
      // Promise.all could run after ROLLBACK, outside the transaction, and persist.
      const end = async () => {
        ended = true;
        await inner.idle();
      };
      await this.connection.execAsync('BEGIN IMMEDIATE');
      try {
        const result = await work(tx);
        await end();
        await this.connection.execAsync('COMMIT');
        return result;
      } catch (error) {
        await end();
        await this.connection.execAsync('ROLLBACK').catch(() => {});
        throw error;
      }
    });
  }

  // Applies migrations generated by drizzle-kit, recording them in Drizzle's own
  // __drizzle_migrations table, the same way drizzle-orm's migrator does.
  migrate({ journal, migrations }: Migrations): Promise<void> {
    return this.queue.run(async () => {
      await this.connection.execAsync(
        'CREATE TABLE IF NOT EXISTS "__drizzle_migrations" (id SERIAL PRIMARY KEY, hash text NOT NULL, created_at numeric)',
      );
      const last = await this.connection.getFirstAsync<{ created_at: number }>(
        'SELECT created_at FROM "__drizzle_migrations" ORDER BY created_at DESC LIMIT 1',
      );
      const pending = journal.entries.filter((entry) => !last || Number(last.created_at) < entry.when);
      if (!pending.length) return;
      await this.connection.execAsync('BEGIN IMMEDIATE');
      try {
        for (const entry of pending) {
          const source = migrations[`m${String(entry.idx).padStart(4, '0')}`];
          if (source === undefined) throw new Error(`Missing migration: ${entry.tag}`);
          for (const statement of source.split('--> statement-breakpoint')) {
            if (statement.trim()) await this.connection.execAsync(statement);
          }
          await this.connection.runAsync('INSERT INTO "__drizzle_migrations" ("hash", "created_at") VALUES (?, ?)', '', entry.when);
        }
        await this.connection.execAsync('COMMIT');
      } catch (error) {
        await this.connection.execAsync('ROLLBACK').catch(() => {});
        throw error;
      }
    });
  }

  // Waits for queued work, then releases the statements and the connection.
  close(): Promise<void> {
    return this.queue.run(async () => {
      if (this.closed) return;
      this.closed = true;
      for (const statement of this.statements.values()) await statement.finalizeAsync().catch(() => {});
      this.statements.clear();
      await this.connection.closeAsync();
    });
  }

  private async statement(query: string): Promise<SQLiteStatement> {
    const cached = this.statements.get(query);
    if (cached) {
      // Move to the most recently used end.
      this.statements.delete(query);
      this.statements.set(query, cached);
      return cached;
    }
    const prepared = await this.connection.prepareAsync(query);
    this.statements.set(query, prepared);
    if (this.statements.size > STATEMENT_CACHE_SIZE) {
      const [oldest, evicted] = this.statements.entries().next().value!;
      this.statements.delete(oldest);
      await evicted.finalizeAsync();
    }
    return prepared;
  }

  // The sqlite-proxy contract: rows come back as value arrays in column order, and
  // `get` returns a single row (or undefined) instead of a list.
  private execute: AsyncRemoteCallback = async (query, params, method) => {
    if (this.closed) throw new Error('The ledger database is closed.');
    const statement = await this.statement(query);
    if (method === 'run') {
      await statement.executeAsync(params);
      return { rows: [] };
    }
    // Read every row, including for `get`: a statement stepped to completion ends its
    // implicit read transaction, so no snapshot stays open between calls.
    const rows = await (await statement.executeForRawResultAsync(params)).getAllAsync();
    return { rows: method === 'get' ? rows[0] : rows } as { rows: unknown[] };
  };
}
