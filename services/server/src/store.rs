use std::{collections::HashSet, path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::ledger::{Entry, Ledger, Record};

const SCHEMA: &str = "
    CREATE TABLE entries (
        local_seq INTEGER PRIMARY KEY,
        id TEXT NOT NULL UNIQUE,
        body TEXT NOT NULL,
        request_body TEXT,
        remote_seq INTEGER UNIQUE,
        discarded INTEGER NOT NULL DEFAULT 0
    ) STRICT;
    CREATE TABLE sync_peer (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        id TEXT NOT NULL
    ) STRICT;
    PRAGMA user_version = 1;
";

pub struct Store(Connection);

/// The ledger has conflicting pending entries, so imports wait until they are resolved.
#[derive(Debug)]
pub struct LedgerBlocked;

impl std::fmt::Display for LedgerBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ledger has pending conflicts")
    }
}

impl std::error::Error for LedgerBlocked {}

pub struct Pending {
    pub record: Record,
    pub conflict: Option<String>,
}

pub struct Snapshot {
    pub ledger: Ledger,
    pub pending: Vec<Pending>,
}

pub struct RemoteEntry {
    pub seq: i64,
    pub record: Record,
    pub body: String,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn =
            Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == 0 {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if version == 0 {
                tx.execute_batch(SCHEMA)?;
            }
            tx.commit()?;
        }
        ensure!(
            version <= 1,
            "database was created by a newer version of peanut-count"
        );
        Ok(Self(conn))
    }

    pub fn add(&mut self, entry: Entry, id: Option<uuid::Uuid>) -> Result<String> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = id.unwrap_or_else(uuid::Uuid::new_v4).to_string();
        let request = serde_json::to_string(&entry)?;
        if let Some((previous, discarded)) = tx
            .query_row(
                "SELECT coalesce(request_body, body), discarded FROM entries WHERE id = ?",
                [&id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()?
        {
            ensure!(!discarded, "entry {id} was discarded");
            ensure!(
                previous == request,
                "entry {id} already exists with different contents"
            );
            return Ok(id);
        }
        let existing = snapshot(&tx)?;
        let mut records = confirmed(&tx)?;
        records.extend(
            existing
                .pending
                .into_iter()
                .filter(|p| p.conflict.is_none())
                .map(|p| p.record),
        );
        records.push(Record {
            id: id.clone(),
            entry,
        });
        let ledger = Ledger::build(records)?;
        let body = serde_json::to_string(ledger.canonical_entry(&id)?)?;
        ensure!(body.len() <= 1_048_576, "booked entry exceeds 1 MiB");
        tx.execute(
            "INSERT INTO entries(id, body, request_body) VALUES(?, ?, ?)",
            params![id, body, request],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Imports a statement atomically. Returns false when the same statement
    /// was already imported, so a concurrent duplicate delivery can tell.
    pub fn import_statement(&mut self, statement: &crate::mobile::Statement) -> Result<bool> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS mobile_statements (
                seq INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, body TEXT NOT NULL
            ) STRICT;",
        )?;
        let body = serde_json::to_string(statement)?;
        if let Some(previous) = tx
            .query_row(
                "SELECT body FROM mobile_statements WHERE id = ?",
                [&statement.id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            ensure!(
                previous == body,
                "statement already imported with different postings"
            );
            return Ok(false);
        }

        let mut records = confirmed(&tx)?;
        for item in snapshot(&tx)?.pending {
            if item.conflict.is_some() {
                return Err(LedgerBlocked.into());
            }
            records.push(item.record);
        }
        let mut accounts: HashSet<String> = records
            .iter()
            .filter_map(|record| {
                if let Entry::Open { account, .. } = &record.entry {
                    Some(account.clone())
                } else {
                    None
                }
            })
            .collect();
        let mut entries = Vec::new();
        let opening = chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();
        for entry in statement.entries()? {
            if let Entry::Transaction { postings, .. } = &entry {
                for posting in postings {
                    if accounts.insert(posting.account.clone()) {
                        entries.push(Entry::Open {
                            date: opening,
                            account: posting.account.clone(),
                            currencies: Vec::new(),
                            booking: Default::default(),
                        });
                    }
                }
            }
            entries.push(entry);
        }
        let new_records: Vec<_> = entries
            .into_iter()
            .map(|entry| Record {
                id: uuid::Uuid::new_v4().to_string(),
                entry,
            })
            .collect();
        records.extend(new_records.iter().cloned());
        Ledger::build(records)?;
        for record in new_records {
            let encoded = serde_json::to_string(&record.entry)?;
            tx.execute(
                "INSERT INTO entries(id, body) VALUES(?, ?)",
                params![record.id, encoded],
            )?;
        }
        tx.execute(
            "INSERT INTO mobile_statements(id, body) VALUES(?, ?)",
            params![statement.id, body],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn has_statement(&self, id: &str) -> Result<bool> {
        let initialized: bool = self.0.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'mobile_statements')",
            [], |row| row.get(0),
        )?;
        Ok(initialized
            && self.0.query_row(
                "SELECT EXISTS(SELECT 1 FROM mobile_statements WHERE id = ?)",
                [id],
                |row| row.get(0),
            )?)
    }

    pub fn statement_page(&self, after: i64) -> Result<Vec<(i64, crate::mobile::Statement)>> {
        let initialized: bool = self.0.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'mobile_statements')",
            [], |row| row.get(0),
        )?;
        if !initialized {
            return Ok(Vec::new());
        }
        let mut stmt = self.0.prepare(
            "SELECT seq, body FROM mobile_statements WHERE seq > ? ORDER BY seq LIMIT 8",
        )?;
        let mut rows = stmt.query([after])?;
        let mut page = Vec::new();
        while let Some(row) = rows.next()? {
            let body: String = row.get(1)?;
            page.push((row.get(0)?, serde_json::from_str(&body)?));
        }
        Ok(page)
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        let tx = self.0.unchecked_transaction()?;
        snapshot(&tx)
    }

    pub fn status(&self) -> Result<Value> {
        let tx = self.0.unchecked_transaction()?;
        let snapshot = snapshot(&tx)?;
        let pending: Vec<_> = snapshot
            .pending
            .iter()
            .map(|item| {
                json!({
                    "id": item.record.id, "entry": item.record.entry, "conflict": item.conflict
                })
            })
            .collect();
        Ok(json!({"remote_sequence": head(&tx)?, "pending": pending}))
    }

    pub fn discard_pending(&self, id: &str) -> Result<()> {
        let changed = self.0.execute(
            "UPDATE entries SET discarded = 1 WHERE id = ? AND remote_seq IS NULL AND discarded = 0",
            [id],
        )?;
        ensure!(changed == 1, "no pending entry {id}");
        Ok(())
    }

    pub fn bind_peer(&self, id: &str) -> Result<()> {
        self.0.execute(
            "INSERT INTO sync_peer VALUES(1, ?) ON CONFLICT DO NOTHING",
            [id],
        )?;
        let current: String = self
            .0
            .query_row("SELECT id FROM sync_peer", [], |row| row.get(0))?;
        ensure!(
            current == id,
            "this local database is already paired with a different remote ledger"
        );
        Ok(())
    }

    pub fn head(&self) -> Result<i64> {
        head(&self.0)
    }

    pub fn confirmed_tip(&self) -> Result<Option<(i64, String, String)>> {
        Ok(self
            .0
            .query_row(
                "SELECT remote_seq, id, body FROM entries
             WHERE remote_seq IS NOT NULL ORDER BY remote_seq DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?)
    }

    pub fn accept(&mut self, page: &[RemoteEntry]) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut cursor = head(&tx)?;
        let mut records = confirmed(&tx)?;
        for item in page {
            if item.seq <= cursor {
                let previous: (String, String) = tx.query_row(
                    "SELECT id, body FROM entries WHERE remote_seq = ?",
                    [item.seq],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                ensure!(
                    previous == (item.record.id.clone(), item.body.clone()),
                    "remote history changed at sequence {}",
                    item.seq
                );
                continue;
            }
            ensure!(
                item.seq == cursor + 1,
                "remote history has a gap at sequence {}",
                cursor + 1
            );
            let count = tx.execute(
                "INSERT INTO entries(id, body, remote_seq) VALUES(?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                     remote_seq = excluded.remote_seq, discarded = 0, body = excluded.body,
                     request_body = CASE WHEN entries.body = excluded.body THEN entries.request_body END
                 WHERE entries.remote_seq IS NULL AND (entries.body = excluded.body OR entries.discarded = 1)",
                params![item.record.id, item.body, item.seq],
            )?;
            ensure!(
                count == 1,
                "remote entry {} conflicts with a local entry using the same ID; discard the pending entry to accept remote history",
                item.record.id
            );
            records.push(item.record.clone());
            cursor = item.seq;
        }
        Ledger::build(records).context("invalid remote ledger; local cursor was preserved")?;
        tx.commit()?;
        Ok(())
    }

    pub fn confirmed_history(&mut self) -> Result<(i64, Vec<Record>)> {
        let tx = self.0.transaction()?;
        Ok((head(&tx)?, confirmed(&tx)?))
    }

    pub fn pending_body(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row(
                "SELECT body FROM entries WHERE id = ? AND remote_seq IS NULL AND discarded = 0",
                [id],
                |row| row.get(0),
            )
            .optional()?)
    }
}

fn head(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT coalesce(max(remote_seq), 0) FROM entries",
        [],
        |row| row.get(0),
    )?)
}

fn confirmed(conn: &Connection) -> Result<Vec<Record>> {
    read_records(
        conn,
        "SELECT id, body FROM entries WHERE remote_seq IS NOT NULL ORDER BY remote_seq",
    )
}

fn read_records(conn: &Connection, sql: &str) -> Result<Vec<Record>> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([])?;
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let body: String = row.get(1)?;
        let entry = serde_json::from_str(&body).with_context(|| format!("decode entry {id}"))?;
        records.push(Record { id, entry });
    }
    Ok(records)
}

fn snapshot(conn: &Connection) -> Result<Snapshot> {
    let confirmed = confirmed(conn)?;
    let queue = read_records(
        conn,
        "SELECT id, body FROM entries WHERE remote_seq IS NULL AND discarded = 0 ORDER BY local_seq",
    )?;
    let mut records = confirmed.clone();
    records.extend(queue.iter().cloned());
    if let Ok(ledger) = Ledger::build(records) {
        let pending = queue
            .into_iter()
            .map(|record| Pending {
                record,
                conflict: None,
            })
            .collect();
        return Ok(Snapshot { ledger, pending });
    }
    let mut records = confirmed;
    let mut ledger = Ledger::build(records.clone()).context("invalid confirmed ledger")?;
    let mut pending = Vec::new();
    for record in queue {
        records.push(record.clone());
        let conflict = match Ledger::build(records.clone()) {
            Ok(valid) => {
                ledger = valid;
                None
            }
            Err(error) => {
                records.pop();
                Some(format!("{error:#}"))
            }
        };
        pending.push(Pending { record, conflict });
    }
    Ok(Snapshot { ledger, pending })
}
