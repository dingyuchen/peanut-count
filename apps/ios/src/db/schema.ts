import { integer, primaryKey, sqliteTable, text } from 'drizzle-orm/sqlite-core';

export interface StatementRow {
  date: string;
  description: string;
  amount: string;
  account: string;
}

export interface Statement {
  id: string;
  source: string;
  currency: string;
  rows: StatementRow[];
}

// Upload state of an imported statement in a sync scope.
export const SENT = 0;
export const PENDING = 1;
// The server holds a different version of the same PDF; both are kept until resolved.
export const CONFLICT = 2;
export type SyncState = typeof SENT | typeof PENDING | typeof CONFLICT;

// Column order matches the tables created before Drizzle, so existing ledgers keep working.
export const statements = sqliteTable('statements', {
  scope: text().notNull(),
  id: text().notNull(),
  body: text({ mode: 'json' }).$type<Statement>().notNull(),
  pending: integer().$type<SyncState>().notNull(),
}, (table) => [primaryKey({ columns: [table.scope, table.id] })]);

export const preferences = sqliteTable('preferences', {
  key: text().primaryKey(),
  value: text().notNull(),
});

export const cursors = sqliteTable('cursors', {
  scope: text().primaryKey(),
  after: integer().notNull(),
});
