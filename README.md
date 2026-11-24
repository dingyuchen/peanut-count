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

## Forwarded transaction emails

Each mobile account can have a secret forwarding address. Forward a bank
alert, card notification, or receipt to it, and the server records the
transaction in that account's ledger. The entry is stored as a one-row
statement, so it reaches the iOS app on the next sync like a PDF import.

The server finds every amount written with a currency symbol or code, every
date, and the phrases that name the other party. A decision model then
answers multiple-choice questions about the email. It decides whether the
email reports a completed transaction and whether money went out or came in.
It chooses which amount, date, and payee apply, which of your asset or
liability accounts was used, and the category. The model only chooses among
values that appear in the email and among your accounts, so it cannot make
up an amount. Name accounts after the card or account number when you can,
for example `Liabilities:Citi-Rewards-1234`, so that "card ending 1234"
matches. Missing accounts and categories are opened automatically. A
category the model is unsure about goes to `Expenses:Uncategorized` or
`Income:Uncategorized`. Correct a wrong entry with a reversal.

The decision model is any server that speaks TypeSafe Jev's `/v1/systemone`
format. This includes hosted Jev and local servers such as LM Studio and
llama.cpp. A hosted model receives the text of every forwarded email.

| Variable | Purpose |
| --- | --- |
| `PEANUT_INBOUND_DOMAIN` | Domain of the forwarding addresses. Email import is off unless it is set. |
| `PEANUT_JEV_URL` | Full decision endpoint URL. Empty by default, and mail is deferred until it is set. |
| `PEANUT_JEV_MODEL` | Decision model ID. |
| `PEANUT_JEV_TOKEN` | Bearer token for a hosted decision API. |
| `PEANUT_INBOUND_TOKEN` | Shared secret for the mail relay webhook. The webhook is off unless it is set. |
| `PEANUT_EMAIL_CURRENCY` | Currency for a bare `$` when the account has no single currency. Defaults to USD. |

The two tokens are read only from the environment. The other variables also
have command-line flags. Run `peanut-count mobile-serve --help`.
Create or replace an account's address with its session token. Replacing an
address disables the old one immediately.

```sh
curl -X POST https://ledger.example.com/v1/inbound-address \
  -H "Authorization: Bearer $SESSION_TOKEN"
# {"address":"0628907df4434180adff5bd10164b173@in.example.com"}
```

Deliver mail in either of two ways.

**Relay webhook.** Use a mail service that can POST the raw message, such as
an email-routing worker or a provider's raw MIME webhook. Point it at
`POST /v1/inbound-email` with the message as the body. Send
`Authorization: Bearer $PEANUT_INBOUND_TOKEN`, and put the envelope recipient
in `X-Peanut-Recipient`. Messages may be up to 4 MiB.

| Status | Meaning | Relay action |
| --- | --- | --- |
| 201 | Imported | Accept |
| 200 | Already imported | Accept |
| 404 | Unknown address | Reject |
| 422 | Not a transaction, or the ledger rejected it. The body has the reason. | Reject |
| 503 | Decision model unset or unreachable, storage busy, ledger has unresolved sync conflicts, or too many deliveries at once | Retry later |

**Local MTA pipe.** `peanut-count ingest-email` reads one message from
standard input. It exits with 0 for an import or duplicate, 67 for an unknown
recipient, 65 for a rejected message, and 75 to defer a retry. For Postfix,
route the domain to a `pipe` transport that delivers one recipient per call.
Otherwise one exit status would acknowledge every recipient of the message.

```
# master.cf
peanut    unix  -       n       n       -       -       pipe
  user=peanut argv=/usr/local/bin/peanut-ingest ${recipient}
# main.cf
peanut_destination_recipient_limit = 1
transport_maps = hash:/etc/postfix/transport
# /etc/postfix/transport
in.example.com  peanut:
```

Also add the domain to `relay_domains` so that Postfix accepts its mail, and
run `postmap /etc/postfix/transport`.

The wrapper exports the variables above and runs the import. It defers
delivery, with exit 75, when it gets anything but one recipient or cannot read
its settings, so Postfix keeps the message. The `peanut` user needs read access
to the settings file and write access to the data directory.

```sh
#!/bin/sh
env=/etc/peanut/inbound.env
[ "$#" -eq 1 ] && [ -r "$env" ] || exit 75
set -a
. "$env" || exit 75
set +a
exec /usr/local/bin/peanut-count ingest-email --data /srv/peanut/mobile-data --recipient "$1"
```

Duplicates are detected by the original message's Message-ID. A relay retry
or a re-forward of the same email as an attachment is recorded once.
Forwarding it inline twice creates a new message, which is recorded twice.
Amounts need a currency symbol or ISO code and a dot decimal separator.
Amounts grouped with spaces or apostrophes, or written with a decimal comma,
are skipped. Each question offers the model at most 50 options. In a larger
ledger, accounts named in the email and accounts of the expected kind come first. An
email that reports several transactions is recorded as one of them.

### Local decision model with LM Studio

LM Studio serves `/v1/systemone` for models it lists as decision models.

```sh
lms get https://huggingface.co/ggml-org/lev-GGUF@Q4_K_M
lms load lev --identifier lev
lms server start
export PEANUT_JEV_URL=http://127.0.0.1:1234/v1/systemone PEANUT_JEV_MODEL=lev
```

`ggml-org/lev-GGUF` is about 3 GB at Q4_K_M. Each email takes two model
calls, about 5 to 8 seconds on an Apple M1 Pro. The `feder-cr/jev` GGUF
(jevos-v4) loads in LM Studio as an ordinary language model, not a decision
model, so its `/v1/systemone` endpoint does not serve it. Run jevos with its
own `jev serve` binary on port 8017 instead. `ggml-org/OpenJev-GGUF` needs
about 20 GB.

## Verification

```sh
cargo test --locked
# With upstream bean-check on PATH.
cargo test --locked --bin peanut-count beancount_accepts_investment_export -- --ignored
# Run against a disposable empty sqld database.
PEANUT_TEST_LIBSQL_URL=http://127.0.0.1:8080 cargo test --locked --test sync_cli -- --ignored
```
