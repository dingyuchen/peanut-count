//! Forwarded transaction emails. Each account can create a secret address at
//! the inbound domain. Mail to it arrives through an HTTP relay or a local MTA
//! pipe, is turned into a one-row statement, and syncs to mobile clients like
//! any other statement import.

use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{
    email::{self, Failure},
    jev::Jev,
    mobile::{self, ApiError, ApiResult, INTERNAL},
    store::Store,
};

/// Largest accepted raw message. Alerts are small; this leaves room for HTML and inline images.
pub const MAX_EMAIL_BYTES: usize = 4 * 1024 * 1024;
/// Relay deliveries processed at once. Each holds up to one message in memory
/// and waits on the decision model, so excess deliveries are deferred.
static RELAY_SLOTS: Semaphore = Semaphore::const_new(4);

#[derive(clap::Args)]
pub struct InboundArgs {
    /// Domain of the per-account forwarding addresses, for example in.example.com.
    /// Email import is off unless this is set.
    #[arg(long, env = "PEANUT_INBOUND_DOMAIN")]
    inbound_domain: Option<String>,
    /// Decision model endpoint in Jev's /v1/systemone format, for example
    /// http://127.0.0.1:1234/v1/systemone. Set PEANUT_JEV_TOKEN for hosted APIs.
    #[arg(long, env = "PEANUT_JEV_URL")]
    jev_url: Option<String>,
    /// Decision model ID, for example typesafe/jev-1.14.
    #[arg(long, env = "PEANUT_JEV_MODEL")]
    jev_model: Option<String>,
    /// Currency for an amount written with a bare "$" when the account has no single currency.
    #[arg(long, env = "PEANUT_EMAIL_CURRENCY", default_value = "USD")]
    email_currency: String,
}

pub struct Inbound {
    domain: String,
    model: Option<Jev>,
    default_currency: String,
}

impl InboundArgs {
    pub fn config(self) -> Result<Option<Inbound>> {
        let set = |value: Option<String>| value.filter(|value| !value.trim().is_empty());
        let Some(domain) = set(self.inbound_domain) else {
            return Ok(None);
        };
        let domain = domain.trim().to_ascii_lowercase();
        ensure!(
            domain.len() <= 253
                && domain.contains('.')
                && domain.split('.').all(|label| !label.is_empty()
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')),
            "invalid inbound domain {domain:?}"
        );
        ensure!(
            self.email_currency.len() == 3
                && self.email_currency.bytes().all(|b| b.is_ascii_uppercase()),
            "email currency must be a three-letter code such as USD"
        );
        let model = match set(self.jev_url) {
            None => None,
            Some(url) => {
                let model = set(self.jev_model).context("set --jev-model or PEANUT_JEV_MODEL")?;
                let token = set(std::env::var("PEANUT_JEV_TOKEN").ok());
                Some(Jev::new(url, model, token)?)
            }
        };
        Ok(Some(Inbound {
            domain,
            model,
            default_currency: self.email_currency,
        }))
    }
}

#[derive(Debug)]
pub enum Outcome {
    Imported(String),
    Duplicate(String),
}

#[derive(Debug)]
pub enum IngestError {
    UnknownRecipient,
    NotConfigured,
    Rejected(String),
    Unavailable(anyhow::Error),
}

/// Imports one raw RFC 5322 message addressed to `recipient`.
pub async fn ingest(
    data: Arc<PathBuf>,
    inbound: Arc<Inbound>,
    recipient: String,
    raw: Bytes,
) -> Result<Outcome, IngestError> {
    use IngestError::*;

    let config = inbound.clone();
    let paths = data.clone();
    let prepared = tokio::task::spawn_blocking(move || -> Result<_, IngestError> {
        let user = recipient_user(&paths, &config.domain, &recipient)?.ok_or(UnknownRecipient)?;
        if config.model.is_none() {
            return Err(NotConfigured);
        }
        let email = email::parse(&raw).map_err(|error| Rejected(format!("{error:#}")))?;
        let store = Store::open(&mobile::ledger_path(&paths, &user)).map_err(Unavailable)?;
        if store.has_statement(&email.id).map_err(Unavailable)? {
            return Ok((user, email, None));
        }
        let ledger = store.snapshot().map_err(Unavailable)?.ledger;
        Ok((
            user,
            email,
            Some((ledger.open_accounts(), ledger.closed_accounts())),
        ))
    })
    .await
    .map_err(|error| Unavailable(error.into()))??;
    let (user, email, accounts) = prepared;
    let Some((accounts, closed)) = accounts else {
        return Ok(Outcome::Duplicate(email.id));
    };

    let model = inbound.model.as_ref().ok_or(NotConfigured)?;
    let statement = email::extract(&email, &accounts, &closed, &inbound.default_currency, model)
        .await
        .map_err(|failure| match failure {
            Failure::Rejected(reason) => Rejected(reason),
            Failure::Unavailable(error) => Unavailable(error),
        })?;

    tokio::task::spawn_blocking(move || {
        let mut store = Store::open(&mobile::ledger_path(&data, &user)).map_err(Unavailable)?;
        match store.import_statement(&statement) {
            Ok(()) => {
                eprintln!(
                    "Imported email statement {} into ledger {user}",
                    statement.id
                );
                Ok(Outcome::Imported(statement.id))
            }
            // A concurrent delivery of the same message won the race.
            Err(_) if store.has_statement(&statement.id).map_err(Unavailable)? => {
                Ok(Outcome::Duplicate(statement.id))
            }
            // Storage errors such as a lock timeout may succeed on retry.
            Err(error) if error.chain().any(|cause| cause.is::<rusqlite::Error>()) => {
                Err(Unavailable(error))
            }
            Err(error) => Err(Rejected(format!(
                "The ledger rejected the transaction: {error:#}"
            ))),
        }
    })
    .await
    .map_err(|error| Unavailable(error.into()))?
}

fn recipient_user(
    data: &Path,
    domain: &str,
    recipient: &str,
) -> Result<Option<String>, IngestError> {
    let address = recipient
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_ascii_lowercase();
    let Some((local, at)) = address.rsplit_once('@') else {
        return Ok(None);
    };
    if at != domain || local.len() != 32 {
        return Ok(None);
    }
    let conn = mobile::registry(data).map_err(|error| IngestError::Unavailable(error.into()))?;
    conn.query_row(
        "SELECT user_id FROM inbound_addresses WHERE address_hash = ?",
        [mobile::digest(local)],
        |row| row.get(0),
    )
    .optional()
    .map_err(|error| IngestError::Unavailable(error.into()))
}

/// Replaces the account's forwarding address and returns the new one.
fn rotate_address(data: &Path, domain: &str, user: &str) -> rusqlite::Result<String> {
    let local = Uuid::new_v4().simple().to_string();
    mobile::registry(data)?.execute(
        "INSERT INTO inbound_addresses VALUES(?, ?)
         ON CONFLICT(user_id) DO UPDATE SET address_hash = excluded.address_hash",
        params![user, mobile::digest(&local)],
    )?;
    Ok(format!("{local}@{domain}"))
}

#[derive(Clone)]
struct InboundState {
    data: Arc<PathBuf>,
    inbound: Arc<Inbound>,
    relay_token: Option<Arc<str>>,
}

pub fn routes(data: Arc<PathBuf>, inbound: Arc<Inbound>, relay_token: Option<String>) -> Router {
    let relay_token = relay_token.filter(|token| !token.is_empty());
    let mut routes = Router::new().route("/v1/inbound-address", post(new_address));
    if relay_token.is_some() {
        routes = routes.route(
            "/v1/inbound-email",
            post(receive).layer(DefaultBodyLimit::max(MAX_EMAIL_BYTES)),
        );
    }
    let state = InboundState {
        data,
        inbound,
        relay_token: relay_token.map(Arc::from),
    };
    // Layers added later run first: authenticate, then take a slot, before reading the body.
    routes
        .route_layer(middleware::from_fn(relay_slot))
        .route_layer(middleware::from_fn_with_state(state.clone(), relay_auth))
        .with_state(state)
}

/// Checks the relay token for /v1/inbound-email before its body is read.
async fn relay_auth(State(state): State<InboundState>, request: Request, next: Next) -> Response {
    if request.uri().path() != "/v1/inbound-email" {
        return next.run(request).await;
    }
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    let expected = state.relay_token.as_deref().unwrap_or_default();
    if expected.is_empty() || !bool::from(token.as_bytes().ct_eq(expected.as_bytes())) {
        return mobile::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}

async fn relay_slot(request: Request, next: Next) -> Response {
    if request.uri().path() != "/v1/inbound-email" {
        return next.run(request).await;
    }
    let Ok(_permit) = RELAY_SLOTS.try_acquire() else {
        return ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Email import is busy. Retry later.",
        )
        .into_response();
    };
    next.run(request).await
}

async fn new_address(
    State(state): State<InboundState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    mobile::blocking(move || {
        let user = mobile::session_user(&state.data, &headers)?;
        let address =
            rotate_address(&state.data, &state.inbound.domain, &user).map_err(|_| INTERNAL)?;
        Ok(Json(json!({ "address": address })))
    })
    .await
}

/// Accepts one raw message from a mail relay that [`relay_auth`] authenticated.
/// The relay names the envelope recipient in `X-Peanut-Recipient`.
async fn receive(State(state): State<InboundState>, headers: HeaderMap, raw: Bytes) -> Response {
    let Some(recipient) = headers
        .get("x-peanut-recipient")
        .and_then(|value| value.to_str().ok())
    else {
        return ApiError(
            StatusCode::BAD_REQUEST,
            "Set X-Peanut-Recipient to the envelope recipient.",
        )
        .into_response();
    };
    match ingest(state.data, state.inbound, recipient.to_owned(), raw).await {
        Ok(Outcome::Imported(id)) => (
            StatusCode::CREATED,
            Json(json!({ "status": "imported", "statement": id })),
        )
            .into_response(),
        Ok(Outcome::Duplicate(id)) => {
            Json(json!({ "status": "duplicate", "statement": id })).into_response()
        }
        Err(IngestError::UnknownRecipient) => {
            ApiError(StatusCode::NOT_FOUND, "Unknown recipient.").into_response()
        }
        Err(IngestError::Rejected(reason)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": reason })),
        )
            .into_response(),
        Err(IngestError::NotConfigured) => ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Email import has no decision model configured.",
        )
        .into_response(),
        Err(IngestError::Unavailable(error)) => {
            eprintln!("Email import failed: {error:#}");
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Email import is temporarily unavailable.",
            )
            .into_response()
        }
    }
}

/// Reads one message from standard input, as a Postfix `pipe` or procmail
/// delivery would, and returns a sysexits code the MTA understands.
/// Configuration and I/O failures defer delivery so the MTA keeps the message.
pub async fn ingest_stdin(
    data: PathBuf,
    inbound: Result<Option<Inbound>>,
    recipient: String,
) -> i32 {
    const EX_DATAERR: i32 = 65;
    const EX_NOUSER: i32 = 67;
    const EX_TEMPFAIL: i32 = 75;

    let prepared = (|| -> Result<_> {
        let inbound = inbound?.context("set --inbound-domain or PEANUT_INBOUND_DOMAIN")?;
        mobile::init_registry(&data)?;
        let mut raw = Vec::new();
        std::io::stdin()
            .take(MAX_EMAIL_BYTES as u64 + 1)
            .read_to_end(&mut raw)
            .context("read message")?;
        Ok((inbound, raw))
    })();
    let (inbound, raw) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!("peanut-count: {error:#}");
            return EX_TEMPFAIL;
        }
    };
    if raw.len() > MAX_EMAIL_BYTES {
        eprintln!("peanut-count: message exceeds 4 MiB");
        return EX_DATAERR;
    }
    {
        match ingest(Arc::new(data), Arc::new(inbound), recipient, raw.into()).await {
            Ok(Outcome::Imported(id)) => {
                println!("{}", json!({ "status": "imported", "statement": id }));
                0
            }
            Ok(Outcome::Duplicate(id)) => {
                println!("{}", json!({ "status": "duplicate", "statement": id }));
                0
            }
            Err(IngestError::UnknownRecipient) => {
                eprintln!("peanut-count: unknown recipient");
                EX_NOUSER
            }
            Err(IngestError::Rejected(reason)) => {
                eprintln!("peanut-count: {reason}");
                EX_DATAERR
            }
            Err(IngestError::NotConfigured) => {
                eprintln!("peanut-count: set PEANUT_JEV_URL and PEANUT_JEV_MODEL to import email");
                EX_TEMPFAIL
            }
            Err(IngestError::Unavailable(error)) => {
                eprintln!("peanut-count: {error:#}");
                EX_TEMPFAIL
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALERT: &str = "Message-ID: <alert-7@bank.example>\r\n\
From: Bank Alerts <alerts@bank.example>\r\n\
Subject: Card transaction alert\r\n\
Date: Tue, 24 Nov 2026 09:15:00 +0800\r\n\
\r\n\
A transaction of S$12.50 was made with your credit card on 23 Nov 2026 at STARBUCKS VIVOCITY.\r\n\
Available balance: SGD 1,204.33.\r\n";

    /// Serves fixed answers in Jev's wire format and returns the endpoint URL.
    async fn fake_model() -> String {
        let app = Router::new().route(
            "/v1/systemone",
            post(|Json(request): Json<serde_json::Value>| async move {
                let has = |name: &str| request["questions"].get(name).is_some();
                let first = ["transaction", "direction", "amount", "date", "payee", "account"];
                assert!(first.iter().all(|q| has(q)) != has("category"));
                Json(json!({ "model": "fake", "answers": {
                    "transaction": { "type": "noul", "noul": 0.98 },
                    "direction": { "type": "choice", "choice": "out", "probabilities": { "out": 0.99, "in": 0.01 } },
                    "amount": { "type": "choice", "choice": "1", "probabilities": { "1": 0.97, "2": 0.03 } },
                    "date": { "type": "choice", "choice": "1", "probabilities": { "1": 0.8, "2": 0.2 } },
                    "payee": { "type": "choice", "choice": "1", "probabilities": { "1": 0.9 } },
                    "account": { "type": "choice", "choice": "Liabilities:CreditCard" },
                    "category": { "type": "choice", "choice": "Expenses:Dining", "probabilities": { "Expenses:Dining": 0.93 } },
                }}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}/v1/systemone")
    }

    fn inbound(model: Option<String>) -> Arc<Inbound> {
        Arc::new(Inbound {
            domain: "in.example.com".into(),
            model: model.map(|url| Jev::new(url, "fake".into(), None).unwrap()),
            default_currency: "USD".into(),
        })
    }

    #[tokio::test]
    async fn forwarded_email_becomes_a_synced_statement() {
        let temp = tempfile::tempdir().unwrap();
        let data = Arc::new(temp.path().to_path_buf());
        mobile::init_registry(&data).unwrap();
        mobile::registry(&data)
            .unwrap()
            .execute("INSERT INTO users VALUES('u1', 'alice', 'unused')", [])
            .unwrap();
        let address = rotate_address(&data, "in.example.com", "u1").unwrap();
        let config = inbound(Some(fake_model().await));
        let deliver =
            |recipient: String| ingest(data.clone(), config.clone(), recipient, Bytes::from(ALERT));

        let Outcome::Imported(id) = deliver(format!("<{}>", address.to_uppercase()))
            .await
            .unwrap()
        else {
            panic!("expected an import");
        };
        let page = Store::open(&mobile::ledger_path(&data, "u1"))
            .unwrap()
            .statement_page(0)
            .unwrap();
        assert_eq!(page.len(), 1);
        let statement = &page[0].1;
        assert_eq!(
            (
                statement.id.as_str(),
                statement.source.as_str(),
                statement.currency.as_str()
            ),
            (id.as_str(), "Liabilities:CreditCard", "SGD")
        );
        let row = &statement.rows[0];
        assert_eq!(
            (
                row.date.to_string(),
                row.description.as_str(),
                row.amount.as_str(),
                row.account.as_str()
            ),
            (
                "2026-11-23".into(),
                "STARBUCKS VIVOCITY",
                "-12.50",
                "Expenses:Dining"
            )
        );

        assert!(
            matches!(deliver(address.clone()).await, Ok(Outcome::Duplicate(same)) if same == id)
        );
        assert!(matches!(
            deliver(address.replace("in.example.com", "other.example")).await,
            Err(IngestError::UnknownRecipient)
        ));
        let unconfigured = ingest(
            data.clone(),
            inbound(None),
            address.clone(),
            Bytes::from(ALERT),
        )
        .await;
        assert!(matches!(unconfigured, Err(IngestError::NotConfigured)));
        rotate_address(&data, "in.example.com", "u1").unwrap();
        assert!(matches!(
            deliver(address).await,
            Err(IngestError::UnknownRecipient)
        ));
    }

    #[tokio::test]
    async fn relay_endpoint_requires_its_token() {
        let temp = tempfile::tempdir().unwrap();
        let data = Arc::new(temp.path().to_path_buf());
        mobile::init_registry(&data).unwrap();
        mobile::registry(&data)
            .unwrap()
            .execute("INSERT INTO users VALUES('u1', 'alice', 'unused')", [])
            .unwrap();
        let address = rotate_address(&data, "in.example.com", "u1").unwrap();
        let app = routes(
            data,
            inbound(Some(fake_model().await)),
            Some("relay-secret".into()),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/inbound-email", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let send = |token: &'static str| {
            client
                .post(&url)
                .bearer_auth(token)
                .header("X-Peanut-Recipient", &address)
                .body(ALERT)
                .send()
        };
        assert_eq!(
            send("wrong").await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send("relay-secret").await.unwrap().status(),
            StatusCode::CREATED
        );
        assert_eq!(send("relay-secret").await.unwrap().status(), StatusCode::OK);
    }
}
