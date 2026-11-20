use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{ledger::Entry, store::Store};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_PAGE_SIZE: usize = 500;

#[derive(Clone)]
struct LedgerTools {
    path: PathBuf,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
    /// Exact account name or parent account prefix, such as Assets.
    account: Option<String>,
    /// Number of matching rows to skip. Defaults to zero.
    offset: Option<usize>,
    /// Number of rows to return, from 1 to 500. Defaults to 100.
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PriceArgs {
    /// Number of matching rows to skip. Defaults to zero.
    offset: Option<usize>,
    /// Number of rows to return, from 1 to 500. Defaults to 100.
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EntryArgs {
    /// Caller-generated UUID. Reuse it when retrying the same entry on this server.
    request_id: String,
    entry: Entry,
}

#[tool_router]
impl LedgerTools {
    fn new(path: PathBuf, write: bool) -> Self {
        let mut tool_router = Self::tool_router();
        if !write {
            tool_router.remove_route("add_entry");
        }
        Self { path, tool_router }
    }

    #[tool(
        description = "List account opening and closing entries.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn accounts(&self, Parameters(args): Parameters<ReportArgs>) -> CallToolResult {
        self.report("accounts", args).await
    }

    #[tool(
        description = "List nonzero account balances by commodity.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn balances(&self, Parameters(args): Parameters<ReportArgs>) -> CallToolResult {
        self.report("balances", args).await
    }

    #[tool(
        description = "List booked transactions, including their postings and reversals.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn transactions(&self, Parameters(args): Parameters<ReportArgs>) -> CallToolResult {
        self.report("transactions", args).await
    }

    #[tool(
        description = "List open investment lots with remaining units and acquisition cost.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn holdings(&self, Parameters(args): Parameters<ReportArgs>) -> CallToolResult {
        self.report("holdings", args).await
    }

    #[tool(
        description = "List realized gains and losses by sale and lot.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn gains(&self, Parameters(args): Parameters<ReportArgs>) -> CallToolResult {
        self.report("gains", args).await
    }

    #[tool(
        description = "List recorded commodity prices.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn prices(&self, Parameters(args): Parameters<PriceArgs>) -> CallToolResult {
        self.report(
            "prices",
            ReportArgs {
                account: None,
                offset: args.offset,
                limit: args.limit,
            },
        )
        .await
    }

    #[tool(
        description = "Validate and record a ledger entry in local SQLite. The next sync submits it to libSQL.",
        annotations(
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn add_entry(&self, Parameters(args): Parameters<EntryArgs>) -> CallToolResult {
        let Ok(id) = Uuid::parse_str(&args.request_id) else {
            return tool_error("request_id must be a UUID");
        };
        let path = self.path.clone();
        run_store(move || {
            let id = Store::open(&path)?.add(args.entry, Some(id))?;
            Ok(json!({ "id": id }))
        })
        .await
    }

    async fn report(&self, name: &'static str, args: ReportArgs) -> CallToolResult {
        let limit = args.limit.unwrap_or(100);
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return tool_error("limit must be between 1 and 500");
        }
        let offset = args.offset.unwrap_or_default();
        let path = self.path.clone();
        run_store(move || {
            let snapshot = Store::open(&path)?.snapshot()?;
            let conflicting_pending = snapshot
                .pending
                .iter()
                .filter(|entry| entry.conflict.is_some())
                .count();
            let Value::Array(items) = snapshot.ledger.report(name, args.account.as_deref())? else {
                anyhow::bail!("ledger report did not return rows");
            };
            let total = items.len();
            let page: Vec<_> = items.into_iter().skip(offset).take(limit).collect();
            let end = offset.saturating_add(page.len());
            Ok(json!({
                "items": page,
                "total": total,
                "next_offset": (end < total).then_some(end),
                "conflicting_pending": conflicting_pending
            }))
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LedgerTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("peanut-count", env!("CARGO_PKG_VERSION")),
        )
    }
}

async fn run_store(work: impl FnOnce() -> Result<Value> + Send + 'static) -> CallToolResult {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(result)) => CallToolResult::structured(result),
        Ok(Err(error)) => tool_error(format!("{error:#}")),
        Err(_) => tool_error("ledger operation did not finish"),
    }
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

async fn authorize(State(token): State<String>, request: Request, next: Next) -> Response {
    let mut headers = request.headers().get_all(header::AUTHORIZATION).iter();
    let supplied = headers
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, supplied)| supplied.as_bytes());
    if headers.next().is_none()
        && supplied.is_some_and(|value| bool::from(token.as_bytes().ct_eq(value)))
    {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
    )
        .into_response()
}

pub async fn serve(
    path: PathBuf,
    addr: SocketAddr,
    token: String,
    write: bool,
    allowed_hosts: Vec<String>,
) -> Result<()> {
    ensure!(
        addr.ip().is_loopback(),
        "MCP must bind to a loopback address. Use a TLS reverse proxy for remote access"
    );
    ensure!(
        !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic()),
        "PEANUT_MCP_TOKEN must contain a nonempty bearer token without whitespace"
    );
    let db_path = path.clone();
    tokio::task::spawn_blocking(move || Store::open(&db_path).map(drop)).await??;
    let tools = LedgerTools::new(path, write);
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .enforce_origin_validation();
    config.max_request_body_bytes = MAX_REQUEST_BYTES;
    config.allowed_hosts.extend(allowed_hosts);
    let service = StreamableHttpService::new(
        move || Ok(tools.clone()),
        NeverSessionManager::default().into(),
        config,
    );
    let router = Router::new()
        .route_service("/mcp", service)
        .layer(middleware::from_fn_with_state(token, authorize));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind MCP endpoint at {addr}"))?;
    eprintln!("MCP listening on http://{}/mcp", listener.local_addr()?);
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serve MCP")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_router_hides_write_tool() {
        assert!(
            !LedgerTools::new(PathBuf::new(), false)
                .tool_router
                .has_route("add_entry")
        );
        assert!(
            LedgerTools::new(PathBuf::new(), true)
                .tool_router
                .has_route("add_entry")
        );
    }
}
