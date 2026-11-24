use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::store::Store;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Statement {
    pub id: String,
    pub source: String,
    pub currency: String,
    pub rows: Vec<StatementRow>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StatementRow {
    pub date: NaiveDate,
    pub description: String,
    pub amount: String,
    pub account: String,
}

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct Session {
    username: String,
    ledger_id: String,
    token: String,
}

#[derive(Deserialize)]
struct Cursor {
    #[serde(default)]
    after: i64,
}

#[derive(Debug)]
pub(crate) struct ApiError(pub StatusCode, pub &'static str);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

pub(crate) const INTERNAL: ApiError =
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Server storage error.");
pub(crate) const UNAUTHORIZED: ApiError = ApiError(
    StatusCode::UNAUTHORIZED,
    "Invalid credentials or expired session.",
);
static AUTH_SLOTS: Semaphore = Semaphore::const_new(4);
pub(crate) type ApiResult<T> = std::result::Result<T, ApiError>;

pub async fn serve(
    listen: SocketAddr,
    data: PathBuf,
    inbound: Option<crate::inbound::Inbound>,
    relay_token: Option<String>,
) -> Result<()> {
    ensure!(
        listen.ip().is_loopback(),
        "mobile API must bind to loopback behind an HTTPS proxy"
    );
    init_registry(&data)?;
    let data = Arc::new(data);
    let mut routes = Router::new()
        .route("/v1/signup", post(signup))
        .route("/v1/login", post(login))
        .route("/v1/statements", post(upload).get(download))
        .route("/v1/health", get(|| async { StatusCode::NO_CONTENT }))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(data.clone());
    if let Some(inbound) = inbound {
        routes = routes.merge(crate::inbound::routes(data, Arc::new(inbound), relay_token));
    }
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!("Mobile API listening on http://{}", listener.local_addr()?);
    axum::serve(listener, routes).await?;
    Ok(())
}

pub(crate) fn init_registry(data: &Path) -> Result<()> {
    std::fs::create_dir_all(data)?;
    registry(data)?.execute_batch("
        CREATE TABLE IF NOT EXISTS users (
            id TEXT PRIMARY KEY, username TEXT NOT NULL UNIQUE, password TEXT NOT NULL
        ) STRICT;
        CREATE TABLE IF NOT EXISTS sessions (
            token_hash TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id), expires INTEGER NOT NULL
        ) STRICT;
        CREATE TABLE IF NOT EXISTS inbound_addresses (
            user_id TEXT PRIMARY KEY REFERENCES users(id), address_hash TEXT NOT NULL UNIQUE
        ) STRICT;
    ")?;
    Ok(())
}

pub(crate) fn registry(data: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(data.join("accounts.sqlite3"))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(conn)
}

async fn signup(
    State(data): State<Arc<PathBuf>>,
    Json(credentials): Json<Credentials>,
) -> ApiResult<Json<Session>> {
    authenticate(data, credentials, true).await
}

async fn login(
    State(data): State<Arc<PathBuf>>,
    Json(credentials): Json<Credentials>,
) -> ApiResult<Json<Session>> {
    authenticate(data, credentials, false).await
}

async fn authenticate(
    data: Arc<PathBuf>,
    credentials: Credentials,
    signup: bool,
) -> ApiResult<Json<Session>> {
    let username = credentials.username.trim().to_ascii_lowercase();
    let password = credentials.password;
    if !(3..=64).contains(&username.len())
        || !username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || !(12..=128).contains(&password.len())
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Use a 3–64 character username and a 12–128 byte password.",
        ));
    }
    let permit = AUTH_SLOTS.try_acquire().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Sign-in service is busy. Try again shortly.",
        )
    })?;
    blocking(move || {
        let _permit = permit;
        let conn = registry(&data).map_err(|_| INTERNAL)?;
        let hasher = Argon2::default();
        let id = if signup {
            let salt = SaltString::encode_b64(Uuid::new_v4().as_bytes()).map_err(|_| INTERNAL)?;
            let hash = hasher
                .hash_password(password.as_bytes(), &salt)
                .map_err(|_| INTERNAL)?
                .to_string();
            let id = Uuid::new_v4().to_string();
            let inserted = conn
                .execute(
                    "INSERT INTO users VALUES(?, ?, ?) ON CONFLICT(username) DO NOTHING",
                    params![id, username, hash],
                )
                .map_err(|_| INTERNAL)?;
            if inserted == 0 {
                return Err(ApiError(StatusCode::CONFLICT, "Username already in use."));
            }
            id
        } else {
            let (id, hash): (String, String) = conn
                .query_row(
                    "SELECT id, password FROM users WHERE username = ?",
                    [&username],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|_| INTERNAL)?
                .ok_or(UNAUTHORIZED)?;
            let hash = PasswordHash::new(&hash).map_err(|_| INTERNAL)?;
            hasher
                .verify_password(password.as_bytes(), &hash)
                .map_err(|_| UNAUTHORIZED)?;
            id
        };
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        conn.execute("DELETE FROM sessions WHERE expires <= unixepoch()", [])
            .map_err(|_| INTERNAL)?;
        conn.execute(
            "INSERT INTO sessions VALUES(?, ?, unixepoch() + 2592000)",
            params![digest(&token), id],
        )
        .map_err(|_| INTERNAL)?;
        Ok(Json(Session {
            username,
            ledger_id: id,
            token,
        }))
    })
    .await
}

pub(crate) fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

/// The account ID for a valid session bearer token.
pub(crate) fn session_user(data: &Path, headers: &HeaderMap) -> ApiResult<String> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| token.len() == 64)
        .ok_or(UNAUTHORIZED)?;
    let conn = registry(data).map_err(|_| INTERNAL)?;
    let id: String = conn
        .query_row(
            "SELECT user_id FROM sessions WHERE token_hash = ? AND expires > unixepoch()",
            [digest(token)],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| INTERNAL)?
        .ok_or(UNAUTHORIZED)?;
    Ok(id)
}

pub(crate) fn ledger_path(data: &Path, id: &str) -> PathBuf {
    data.join(format!("{id}.db"))
}

fn ledger(data: &Path, headers: &HeaderMap) -> ApiResult<Store> {
    let id = session_user(data, headers)?;
    Store::open(&ledger_path(data, &id)).map_err(|_| INTERNAL)
}

async fn upload(
    State(data): State<Arc<PathBuf>>,
    headers: HeaderMap,
    Json(statement): Json<Statement>,
) -> ApiResult<StatusCode> {
    blocking(move || {
        let mut store = ledger(&data, &headers)?;
        store.import_statement(&statement).map_err(|_| ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "This statement conflicts with an existing import or contains invalid transactions.",
        ))?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn download(
    State(data): State<Arc<PathBuf>>,
    headers: HeaderMap,
    Query(cursor): Query<Cursor>,
) -> ApiResult<Json<serde_json::Value>> {
    blocking(move || {
        if cursor.after < 0 {
            return Err(ApiError(StatusCode::BAD_REQUEST, "Invalid sync cursor."));
        }
        let rows = ledger(&data, &headers)?
            .statement_page(cursor.after)
            .map_err(|_| INTERNAL)?;
        let after = rows.last().map_or(cursor.after, |(seq, _)| *seq);
        let statements: Vec<_> = rows.into_iter().map(|(_, statement)| statement).collect();
        Ok(Json(json!({ "after": after, "statements": statements })))
    })
    .await
}

pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| INTERNAL)?
}

impl Statement {
    pub fn entries(&self) -> Result<Vec<crate::ledger::Entry>> {
        use crate::ledger::{Amount, Entry, Posting};

        ensure!(
            self.id.len() == 64
                && self
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "invalid statement fingerprint"
        );
        ensure!(
            (1..=500).contains(&self.rows.len()),
            "invalid statement length"
        );
        ensure!(
            self.source.starts_with("Assets:") || self.source.starts_with("Liabilities:"),
            "statement account must be an asset or liability"
        );
        let first_date = NaiveDate::from_ymd_opt(1900, 1, 1).context("invalid opening date")?;
        self.rows
            .iter()
            .map(|row| {
                ensure!(
                    row.date >= first_date
                        && row.description.len() <= 512
                        && !row.description.trim().is_empty(),
                    "invalid statement row"
                );
                ensure!(
                    row.amount.len() <= 24 && row.account != self.source,
                    "invalid posting"
                );
                let magnitude = row.amount.strip_prefix('-').unwrap_or(&row.amount);
                ensure!(
                    magnitude.bytes().any(|b| matches!(b, b'1'..=b'9')),
                    "zero amount"
                );
                let opposite = if row.amount.starts_with('-') {
                    magnitude.to_owned()
                } else {
                    format!("-{magnitude}")
                };
                let posting = |account: String, number: String| Posting {
                    account,
                    units: Amount {
                        number,
                        currency: self.currency.clone(),
                    },
                    cost: None,
                    price: None,
                    sell: false,
                    lot: None,
                };
                Ok(Entry::Transaction {
                    date: row.date,
                    payee: None,
                    narration: row.description.clone(),
                    postings: vec![
                        posting(self.source.clone(), row.amount.clone()),
                        posting(row.account.clone(), opposite),
                    ],
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statement() -> Statement {
        Statement {
            id: "a".repeat(64),
            source: "Assets:Checking".into(),
            currency: "USD".into(),
            rows: vec![StatementRow {
                date: NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
                description: "Coffee".into(),
                amount: "-4.75".into(),
                account: "Expenses:Dining".into(),
            }],
        }
    }

    #[test]
    fn statement_import_is_atomic_balanced_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&temp.path().join("ledger.db")).unwrap();
        let mut item = statement();
        store.import_statement(&item).unwrap();
        store.import_statement(&item).unwrap();
        assert_eq!(store.statement_page(0).unwrap().len(), 1);

        item.rows[0].account = "Expenses:Different".into();
        assert!(store.import_statement(&item).is_err());
        item.id = "b".repeat(64);
        item.rows.push(StatementRow {
            account: "invalid".into(),
            ..item.rows[0].clone()
        });
        assert!(store.import_statement(&item).is_err());
        assert_eq!(store.statement_page(0).unwrap().len(), 1);

        let snapshot = store.snapshot().unwrap();
        assert_eq!(
            snapshot
                .ledger
                .report("transactions", None)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            snapshot.ledger.report("balances", None).unwrap(),
            json!([
                { "account": "Assets:Checking", "currency": "USD", "number": "-4.75" },
                { "account": "Expenses:Dining", "currency": "USD", "number": "4.75" }
            ])
        );
    }
}
