use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result, bail, ensure};
use libsql::{Builder, Connection, params};
use url::Url;

use crate::{
    ledger::{Ledger, Record},
    store::{RemoteEntry, Store},
};

const PAGE_SIZE: i64 = 200;
const PAGE_BYTES: i64 = 2 * 1024 * 1024;
const MAX_HEAD_RETRIES: usize = 8;

pub async fn sync(path: &Path, endpoint: &str, token: &str) -> Result<()> {
    validate_endpoint(endpoint)?;
    let remote = Builder::new_remote(endpoint.to_owned(), token.to_owned())
        .build()
        .await?;
    let conn = remote.connect()?;
    let mut local = Store::open(path)?;
    let tip = local.confirmed_tip()?;
    let (id, remote_head) = prepare_remote(&conn).await?;
    local.bind_peer(&id)?;
    if let Some((seq, id, body)) = tip {
        ensure!(
            remote_head >= seq,
            "remote ledger is behind the local cursor"
        );
        let mut rows = conn
            .query("SELECT id, body FROM peanut_entries WHERE seq = ?", [seq])
            .await?;
        let row = rows
            .next()
            .await?
            .context("remote ledger is missing the local cursor entry")?;
        ensure!(
            row.get::<String>(0)? == id && row.get::<String>(1)? == body,
            "remote history changed at sequence {seq}"
        );
    }
    pull(&conn, &mut local, remote_head).await?;

    // Use a fixed queue for this run. Entries added concurrently are picked up next time.
    let queue = local.snapshot()?.pending;
    let mut blocked = HashMap::new();
    for item in queue {
        let id = &item.record.id;
        for attempt in 0..MAX_HEAD_RETRIES {
            let Some(body) = local.pending_body(id)? else {
                break;
            };
            // The observed head must come from the same read of confirmed history.
            let (head, mut records) = local.confirmed_history()?;
            records.push(item.record.clone());
            if let Err(error) = Ledger::build(records) {
                blocked.insert(id.clone(), format!("{error:#}"));
                break;
            }

            // The conditional insert makes validation safe across concurrent authors.
            let changed = conn
                .execute(
                    "INSERT INTO peanut_entries(seq, id, body)
                    SELECT ? + 1, ?, ?
                    WHERE (SELECT coalesce(max(seq), 0) FROM peanut_entries) = ?
                    ON CONFLICT DO NOTHING",
                    params![head, id.clone(), body.clone(), head],
                )
                .await
                .context("push entry to libSQL")?;
            if changed != 0 {
                local.accept(&[RemoteEntry {
                    seq: head + 1,
                    record: item.record.clone(),
                    body,
                }])?;
                break;
            }
            let remote_head = head_on_remote(&conn).await?;
            ensure!(
                remote_head >= head,
                "remote ledger is behind the local cursor"
            );
            pull(&conn, &mut local, remote_head).await?;
            if local.pending_body(id)?.is_none() {
                break;
            }
            if attempt + 1 == MAX_HEAD_RETRIES {
                bail!("remote ledger kept changing during sync; retry to continue");
            }
        }
    }
    pull(&conn, &mut local, head_on_remote(&conn).await?).await?;
    let pending = local.snapshot()?.pending;
    if !pending.is_empty() {
        let reasons = pending
            .iter()
            .map(|item| {
                format!(
                    "{}: {}",
                    item.record.id,
                    item.conflict
                        .as_deref()
                        .or_else(|| blocked.get(&item.record.id).map(String::as_str))
                        .unwrap_or("queued for the next sync")
                )
            })
            .collect::<Vec<_>>();
        bail!(
            "sync reached sequence {}; pending entries:\n{}",
            local.head()?,
            reasons.join("\n")
        );
    }
    println!("Synced through sequence {}", local.head()?);
    Ok(())
}

async fn prepare_remote(conn: &Connection) -> Result<(String, i64)> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS peanut_ledger (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1), id TEXT NOT NULL, format INTEGER NOT NULL
        )", (),
    ).await.context("initialize libSQL ledger metadata")?;
    conn.execute(
        "INSERT INTO peanut_ledger VALUES (1, ?, 1) ON CONFLICT DO NOTHING",
        [uuid::Uuid::new_v4().to_string()],
    )
    .await?;
    let mut rows = conn
        .query(
            "SELECT id, format FROM peanut_ledger WHERE singleton = 1",
            (),
        )
        .await?;
    let row = rows
        .next()
        .await?
        .context("remote ledger metadata is missing")?;
    let id: String = row.get(0)?;
    let version: i64 = row.get(1)?;
    ensure!(version == 1, "unsupported remote ledger format {version}");
    uuid::Uuid::parse_str(&id).context("invalid remote ledger ID")?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS peanut_entries (
            seq INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, body TEXT NOT NULL
        )",
        (),
    )
    .await
    .context("initialize libSQL entry log")?;
    Ok((id, head_on_remote(conn).await?))
}

async fn head_on_remote(conn: &Connection) -> Result<i64> {
    let mut rows = conn
        .query("SELECT coalesce(max(seq), 0) FROM peanut_entries", ())
        .await?;
    Ok(rows
        .next()
        .await?
        .context("remote ledger head is missing")?
        .get(0)?)
}

async fn pull(conn: &Connection, local: &mut Store, through: i64) -> Result<()> {
    loop {
        let head = local.head()?;
        if head >= through {
            return Ok(());
        }
        let mut rows = conn.query(
            "WITH page AS (
                 SELECT seq, id, body FROM peanut_entries WHERE seq > ? AND seq <= ? ORDER BY seq LIMIT ?
             ), sized AS (
                 SELECT seq, id, body, sum(length(CAST(body AS BLOB))) OVER (ORDER BY seq) AS bytes
                 FROM page
             )
             SELECT seq, id, body FROM sized WHERE bytes <= ? ORDER BY seq",
            params![head, through, PAGE_SIZE, PAGE_BYTES],
        ).await.context("pull entries from libSQL")?;
        let mut page = Vec::new();
        while let Some(row) = rows.next().await? {
            let seq = row.get(0)?;
            let id: String = row.get(1)?;
            let body: String = row.get(2)?;
            uuid::Uuid::parse_str(&id).context("invalid remote entry ID")?;
            let entry =
                serde_json::from_str(&body).with_context(|| format!("decode remote entry {id}"))?;
            page.push(RemoteEntry {
                seq,
                body,
                record: Record { id, entry },
            });
        }
        ensure!(
            !page.is_empty(),
            "remote history ended before sequence {through}"
        );
        local.accept(&page)?;
    }
}

fn validate_endpoint(value: &str) -> Result<()> {
    let url = Url::parse(value).context("invalid libSQL URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https" | "libsql")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none(),
        "use a libsql://, https://, or http:// URL with a host and optional port"
    );
    Ok(())
}
