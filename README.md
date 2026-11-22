# peanut-count

A double-entry ledger for a small server and its clients. The Rust executable
bundles SQLite and serves MCP directly. Each device has its own SQLite file and
can record entries offline. An optional self-hosted libSQL server (sqld) holds
the shared history. Beancount text is generated on demand.

## Repository layout

| Directory | Component |
| --- | --- |
| [services/server](services/server) | Rust CLI, ledger, sync, and MCP server |
| [apps/ios](apps/ios) | React Native iOS client with PDF statement import |
| [training/statement-parser](training/statement-parser) | Training and evaluation for statement-row classification |

The root Cargo workspace keeps builds and tests available from the repo root.

## Build and start a ledger

Build with Rust 1.89 or newer and a C compiler for the bundled SQLite library.

```sh
cargo build --release --locked
export PATH="$PWD/target/release:$PATH"
umask 077
export PEANUT_DB="$PWD/ledger.db"
peanut-count init
```

The installed executable needs neither Rust nor an external SQLite library.
For a statically linked Linux build, build on Alpine with Rust and the
`build-base` package installed. Pass `--db PATH` or set `PEANUT_DB` to select a ledger.

## Record entries and investments

Pipe one JSON directive to `add`. Keep all amounts in decimal strings. The
returned UUID identifies the entry. To retry the same request on the same local
database, supply a UUID with `add --id UUID` on the first attempt and reuse it.

```sh
peanut-count add <<'JSON'
{"type":"open","date":"2026-01-01","account":"Assets:Brokerage","currencies":["ACME"],"booking":"FIFO"}
JSON
peanut-count add <<'JSON'
{"type":"open","date":"2026-01-01","account":"Assets:Cash","currencies":["USD"]}
JSON
peanut-count add <<'JSON'
{"type":"open","date":"2026-01-01","account":"Income:Capital-Gains","currencies":["USD"]}
JSON
peanut-count add <<'JSON'
{
  "type": "transaction", "date": "2026-01-10", "narration": "Buy ACME",
  "postings": [
    {"account":"Assets:Brokerage","units":{"number":"1.5","currency":"ACME"},"cost":{"number":"100","currency":"USD"}},
    {"account":"Assets:Cash","units":{"number":"-150","currency":"USD"}}
  ]
}
JSON
peanut-count add <<'JSON'
{
  "type": "transaction", "date": "2026-02-10", "narration": "Sell ACME",
  "postings": [
    {"account":"Assets:Brokerage","units":{"number":"-0.5","currency":"ACME"},"sell":true,"price":{"number":"120","currency":"USD"}},
    {"account":"Assets:Cash","units":{"number":"60","currency":"USD"}},
    {"account":"Income:Capital-Gains","units":{"number":"-10","currency":"USD"}}
  ]
}
JSON
peanut-count report holdings
peanut-count report gains
peanut-count report transactions --account Assets:Brokerage
peanut-count report trial-balance
```

`cost` and `price` are per unit. An acquisition creates a lot. A sale can
span several lots, and you can provide `lot` from the holdings report to
select one explicitly. Accounts use `STRICT` booking by default. It accepts
an unambiguous lot or a sale of every eligible unit. `FIFO` books the oldest
lots first. The booking is stored with the entry, so future backdated entries
and sync cannot silently select a different lot. The gains report shows gross
realized gains per lot in the cost currency. Record fees separately. The
trial balance uses posting weights, and the balances report shows units.

Additional directives use the same `add` command.

| Type | Fields besides `type` and `date` |
| --- | --- |
| `close` | `account` |
| `commodity` | `currency` |
| `balance` | `account`, `amount` (checked at the start of the date, including descendants) |
| `price` | `currency`, `amount` (quote for one unit of the commodity) |
| `reverse` | `target` (original transaction UUID) |

Transactions can also include a `payee`. A reversal uses opposite postings
and the same lots. Reversing a purchase requires enough units in that lot.
The supported investment workflow covers long lots acquired at a positive
cost and sold in the cost currency. Beancount plugins, lot transfers, short
selling with cost basis, and BQL are outside this release.

```sh
peanut-count export > ledger.beancount
bean-check ledger.beancount
```

The exporter includes booked costs and lot labels, and checks that the data
fits a conservative bound for Beancount's decimal precision. Reports and export include valid pending
local entries. Export stops when unresolved sync conflicts would leave entries
out. The SQLite file remains the ledger, and `bean-check` is optional
verification of the exported file.

## Sync through self-hosted libSQL

Use a dedicated sqld database. The following runs a local instance with
persistent storage and exposes it only on the host loopback interface.

```sh
docker run --rm --name peanut-libsql \
  -p 127.0.0.1:8080:8080 \
  -v peanut-libsql:/var/lib/sqld \
  --entrypoint /bin/sqld ghcr.io/tursodatabase/libsql-server:latest \
  --http-listen-addr 0.0.0.0:8080 --db-path /var/lib/sqld/ledger.db
```

Use the `:latest-arm` tag for ARM64. For remote access, forward the
loopback port over SSH or place sqld behind authenticated HTTPS. Use sqld's
JWT authentication when network clients connect directly, and set
`PEANUT_LIBSQL_TOKEN` on those clients. Every sync client with database
write access is trusted to follow the ledger protocol. See the
[libSQL self-hosting guide](https://github.com/tursodatabase/libsql/blob/main/docs/USER_GUIDE.md).

```sh
export PEANUT_LIBSQL_URL=http://127.0.0.1:8080
peanut-count --db server.db sync
peanut-count --db client.db sync
peanut-count --db client.db status
```

Each sync pulls the shared history and pushes compatible queued entries.
The remote sequence orders accepted changes and ensures that concurrent
authors validate against the same history they extend. For example, if
two clients consume the same investment lot offline, the later conflicting
sale stays pending on its originating device. Inspect it with `status`.
Use `discard-pending UUID` to withdraw that draft and record a corrected
entry. Correct a confirmed transaction with a reversal. Schedule the same
`sync` command on the server to keep its MCP ledger current. Back up the
sqld ledger and any local databases containing unsynced work.

## MCP server

```sh
export PEANUT_MCP_TOKEN="$(openssl rand -hex 32)"
peanut-count --db server.db serve
```

Configure your MCP client for Streamable HTTP at
`http://127.0.0.1:8765/mcp` with `Authorization: Bearer <token>`. The
endpoint exposes paged tools for accounts, balances, transactions, holdings,
gains, and prices. Pass `--allow-write` to expose `add_entry`, which takes
a caller-generated `request_id` UUID and the same `entry` object as the CLI.
Reuse the request ID when retrying against the same server. Reports include
the count of conflicting pending entries excluded from the visible ledger.

The process binds to loopback. Use a TLS reverse proxy for remote clients
that can send a configured bearer token. Add the public hostname with
`--allow-host` if the proxy preserves it. Requests from browser origins are
rejected. This server uses a configured bearer token, so connect from an MCP
client that supports custom authorization headers.

## Mobile accounts and statement sync

The iOS client can keep a ledger on the device or sign in to a mobile sync
server. Start the mobile API with a private data directory:

```sh
umask 077
peanut-count mobile-serve --data ./mobile-data
```

Expose the loopback listener at port 8766 behind an HTTPS reverse proxy.
Configure login and signup rate limits at the proxy before exposing it.
The app accepts the HTTPS base URL when the user signs up or signs in.
Each account has an isolated SQLite ledger. The API returns its ledger ID
at signup and sign-in. An operator can use the CLI with
`--db mobile-data/<ledger-id>.db` to report on it, export it, or sync its
entries to a dedicated self-hosted libSQL database. Mobile devices exchange
statement imports through the mobile API.

See the [iOS setup guide](apps/ios/README.md) for the share and import flow.

## Verification

```sh
cargo test --locked
# With upstream bean-check on PATH.
cargo test --locked --bin peanut-count beancount_accepts_investment_export -- --ignored
# Run against a disposable empty sqld database.
PEANUT_TEST_LIBSQL_URL=http://127.0.0.1:8080 cargo test --locked --test sync_cli -- --ignored
```
