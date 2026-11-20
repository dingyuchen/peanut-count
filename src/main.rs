mod ledger;
mod mcp;
mod store;
mod sync;

#[cfg(test)]
mod ledger_tests;

use std::{
    io::{self, BufWriter, Read, Write},
    net::SocketAddr,
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;
use uuid::Uuid;

use crate::store::Store;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Local SQLite database file.
    #[arg(
        long,
        global = true,
        env = "PEANUT_DB",
        default_value = "peanut-count.db"
    )]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a local ledger.
    Init,
    /// Record one JSON directive from standard input.
    Add {
        /// Stable UUID to safely retry the same request.
        #[arg(long)]
        id: Option<Uuid>,
    },
    /// Read a report as JSON.
    Report {
        #[arg(value_enum)]
        report: Report,
        /// Include this account and its descendants.
        #[arg(long)]
        account: Option<String>,
    },
    /// Validate the visible ledger and print its Beancount representation.
    Export,
    /// Exchange entries with the shared libSQL ledger.
    Sync {
        #[arg(long, env = "PEANUT_LIBSQL_URL")]
        url: String,
    },
    /// Show the confirmed remote sequence and local entries awaiting sync.
    Status,
    /// Withdraw a pending entry from the local queue.
    DiscardPending { id: Uuid },
    /// Serve the Streamable HTTP MCP endpoint.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8765")]
        listen: SocketAddr,
        /// Advertise the ledger write tool.
        #[arg(long)]
        allow_write: bool,
        /// Accept this additional Host header, for example behind an HTTPS proxy.
        #[arg(long)]
        allow_host: Vec<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Report {
    Accounts,
    Balances,
    TrialBalance,
    Transactions,
    Holdings,
    Gains,
    Prices,
}

impl Report {
    fn name(self) -> &'static str {
        match self {
            Self::Accounts => "accounts",
            Self::Balances => "balances",
            Self::TrialBalance => "trial-balance",
            Self::Transactions => "transactions",
            Self::Holdings => "holdings",
            Self::Gains => "gains",
            Self::Prices => "prices",
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let Cli { db, command } = Cli::parse();
    match command {
        Command::Init => {
            Store::open(&db)?;
        }
        Command::Add { id } => {
            const MAX_ENTRY_BYTES: u64 = 1_048_576;
            let mut input = Vec::new();
            io::stdin()
                .take(MAX_ENTRY_BYTES + 1)
                .read_to_end(&mut input)?;
            ensure!(input.len() as u64 <= MAX_ENTRY_BYTES, "entry exceeds 1 MiB");
            let entry = serde_json::from_slice(&input).context("read one JSON ledger directive")?;
            let id = Store::open(&db)?.add(entry, id)?;
            println!("{id}");
        }
        Command::Report { report, account } => {
            let snapshot = Store::open(&db)?.snapshot()?;
            let conflicts = snapshot
                .pending
                .iter()
                .filter(|item| item.conflict.is_some())
                .count();
            if conflicts != 0 {
                eprintln!(
                    "{conflicts} conflicting pending entries were excluded. Run peanut-count status."
                );
            }
            print_json(&snapshot.ledger.report(report.name(), account.as_deref())?)?;
        }
        Command::Export => {
            let snapshot = Store::open(&db)?.snapshot()?;
            ensure!(
                snapshot.pending.iter().all(|item| item.conflict.is_none()),
                "ledger has conflicting pending entries; run status and resolve them before exporting"
            );
            let mut out = BufWriter::new(io::stdout().lock());
            snapshot.ledger.validate_export()?;
            out.write_all(snapshot.ledger.export().as_bytes())?;
            out.flush()?;
        }
        Command::Sync { url } => {
            let token = match std::env::var("PEANUT_LIBSQL_TOKEN") {
                Ok(value) => value,
                Err(std::env::VarError::NotPresent) => String::new(),
                Err(error) => return Err(error).context("read PEANUT_LIBSQL_TOKEN"),
            };
            sync::sync(&db, &url, &token).await?;
        }
        Command::Status => print_json(&Store::open(&db)?.status()?)?,
        Command::DiscardPending { id } => Store::open(&db)?.discard_pending(&id.to_string())?,
        Command::Serve {
            listen,
            allow_write,
            allow_host,
        } => {
            let token = std::env::var("PEANUT_MCP_TOKEN").context("set PEANUT_MCP_TOKEN")?;
            mcp::serve(db, listen, token, allow_write, allow_host).await?;
        }
    }
    Ok(())
}

fn print_json(value: &Value) -> Result<()> {
    let mut out = BufWriter::new(io::stdout().lock());
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}
