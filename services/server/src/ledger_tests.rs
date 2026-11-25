use std::{fs, process::Command};

use serde_json::{Value, json};

use crate::ledger::{Ledger, Record};

fn record(id: &str, entry: Value) -> Record {
    Record {
        id: id.into(),
        entry: serde_json::from_value(entry).unwrap(),
    }
}

fn amount(number: &str, currency: &str) -> Value {
    json!({ "number": number, "currency": currency })
}

fn posting(account: &str, number: &str, currency: &str) -> Value {
    json!({ "account": account, "units": amount(number, currency) })
}

fn open(account: &str, currency: &str, booking: &str) -> Record {
    record(
        account,
        json!({
            "type": "open",
            "date": "2026-01-01",
            "account": account,
            "currencies": [currency],
            "booking": booking
        }),
    )
}

fn transaction(id: &str, date: &str, postings: Vec<Value>) -> Record {
    record(
        id,
        json!({
            "type": "transaction",
            "date": date,
            "narration": id,
            "postings": postings
        }),
    )
}

fn sale(id: &str, date: &str, units: &str, cash: &str, gain: &str, lot: Option<&str>) -> Record {
    let mut shares = posting("Assets:Broker", units, "ACME");
    shares["sell"] = json!(true);
    shares["price"] = amount("15", "USD");
    if let Some(lot) = lot {
        shares["lot"] = json!(lot);
    }
    transaction(
        id,
        date,
        vec![
            shares,
            posting("Assets:Cash", cash, "USD"),
            posting("Income:Gains", gain, "USD"),
        ],
    )
}

fn investments(booking: &str) -> Vec<Record> {
    let mut first = posting("Assets:Broker", "2.5", "ACME");
    first["cost"] = amount("10", "USD");
    let mut second = posting("Assets:Broker", "3", "ACME");
    second["cost"] = amount("12", "USD");
    vec![
        open("Assets:Broker", "ACME", booking),
        open("Assets:Cash", "USD", "STRICT"),
        open("Income:Gains", "USD", "STRICT"),
        record(
            "acme",
            json!({ "type": "commodity", "date": "2026-01-01", "currency": "ACME" }),
        ),
        record(
            "assert-opening",
            json!({
                "type": "balance", "date": "2026-01-01",
                "account": "Assets:Cash", "amount": amount("0", "USD")
            }),
        ),
        record(
            "buy-first",
            json!({
                "type": "transaction", "date": "2026-01-01",
                "payee": r#"A"B\C"#,
                "narration": r#"Cost in "USD""#,
                "postings": [first, posting("Assets:Cash", "-25", "USD")]
            }),
        ),
        transaction(
            "buy-second",
            "2026-01-02",
            vec![second, posting("Assets:Cash", "-36", "USD")],
        ),
    ]
}

fn report(ledger: &Ledger, kind: &str) -> Vec<Value> {
    ledger
        .report(kind, None)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

fn error(records: Vec<Record>) -> String {
    match Ledger::build(records) {
        Ok(_) => panic!("invalid ledger was accepted"),
        Err(error) => format!("{error:#}"),
    }
}

#[test]
fn exact_balancing_and_beginning_of_day_assertions() {
    let transfer = transaction(
        "transfer",
        "2026-01-01",
        vec![
            posting("Assets:Cash", "0.100000000000000001", "USD"),
            posting("Assets:Cash", "0.200000000000000002", "USD"),
            posting("Equity:Opening", "-0.300000000000000003", "USD"),
        ],
    );
    let mut records = vec![
        transfer,
        open("Assets:Cash", "USD", "STRICT"),
        open("Equity:Opening", "USD", "STRICT"),
        record(
            "initial-balance",
            json!({
                "type": "balance", "date": "2026-01-01",
                "account": "Assets:Cash", "amount": amount("0", "USD")
            }),
        ),
    ];
    let ledger = Ledger::build(records.clone()).unwrap();
    assert_eq!(
        ledger.report("balances", Some("Assets")).unwrap(),
        json!([{"account":"Assets:Cash","currency":"USD","number":"0.300000000000000003"}]),
    );
    let export = ledger.export();
    assert!(export.find("balance Assets:Cash").unwrap() < export.find("* \"transfer\"").unwrap());

    records[0] = transaction(
        "unbalanced",
        "2026-01-01",
        vec![
            posting("Assets:Cash", "0.300000000000000004", "USD"),
            posting("Equity:Opening", "-0.300000000000000003", "USD"),
        ],
    );
    assert!(error(records).contains("out of balance by 0.000000000000000001 USD"));
}

#[test]
fn exact_ledger_amounts_outside_beancount_precision_are_not_exportable() {
    let number = "12345678901234567890123456789012345678.123456789012345678";
    let records = vec![
        open("Assets:Cash", "USD", "STRICT"),
        open("Equity:Opening", "USD", "STRICT"),
        transaction(
            "transfer",
            "2026-01-01",
            vec![
                posting("Assets:Cash", number, "USD"),
                posting("Equity:Opening", &format!("-{number}"), "USD"),
            ],
        ),
    ];
    let ledger = Ledger::build(records).unwrap();
    assert_eq!(report(&ledger, "balances")[0]["number"], number);
    assert!(ledger.validate_export().is_err());
}

#[test]
fn fifo_lots_are_booked_once_and_survive_a_backdated_acquisition() {
    let mut records = investments("FIFO");
    records.push(sale("sale", "2026-01-03", "-3", "45", "-14", None));
    let ledger = Ledger::build(records.clone()).unwrap();
    let lots = report(&ledger, "holdings");
    assert_eq!(lots.len(), 1);
    assert_eq!(lots[0]["lot"], "lot-buy-second-1");
    assert_eq!(lots[0]["units"], "2.5");
    assert_eq!(
        ledger.report("trial-balance", None).unwrap(),
        json!([
            {"account": "Assets:Broker", "currency": "USD", "number": "30"},
            {"account": "Assets:Cash", "currency": "USD", "number": "-16"},
            {"account": "Income:Gains", "currency": "USD", "number": "-14"}
        ]),
    );
    let gains = report(&ledger, "gains");
    assert_eq!(gains.len(), 2);
    assert_eq!(gains[0]["number"], "12.5");
    assert_eq!(gains[1]["number"], "1.5");

    records.last_mut().unwrap().entry = ledger.canonical_entry("sale").unwrap().clone();
    let mut backdated = posting("Assets:Broker", "1", "ACME");
    backdated["cost"] = amount("11", "USD");
    records.push(transaction(
        "backdated",
        "2026-01-01",
        vec![backdated, posting("Assets:Cash", "-11", "USD")],
    ));
    let rebuilt = Ledger::build(records).unwrap();
    assert_eq!(report(&rebuilt, "gains"), gains);
    assert_eq!(report(&rebuilt, "holdings")[0]["lot"], "lot-backdated-1");
}

#[test]
fn strict_booking_requires_an_explicit_lot_and_rejects_oversells() {
    let mut records = investments("STRICT");
    records.push(sale("ambiguous", "2026-01-03", "-1", "15", "-3", None));
    assert!(error(records.clone()).contains("ambiguous sale"));

    records.pop();
    let lot = Some("lot-buy-second-1");
    records.push(sale("explicit", "2026-01-03", "-1", "15", "-3", lot));
    let ledger = Ledger::build(records.clone()).unwrap();
    assert_eq!(report(&ledger, "holdings")[1]["units"], "2");
    assert_eq!(report(&ledger, "gains")[0]["lot"], "lot-buy-second-1");
    records.push(sale("oversell", "2026-01-04", "-3", "45", "-9", lot));
    assert!(error(records).contains("insufficient lots"));
}

#[test]
fn plain_postings_cannot_change_an_account_with_costed_units() {
    let mut records = investments("FIFO");
    records.push(open("Equity:Opening", "ACME", "STRICT"));
    let transfer = |date| {
        transaction(
            "plain-transfer",
            date,
            vec![
                posting("Assets:Broker", "-1", "ACME"),
                posting("Equity:Opening", "1", "ACME"),
            ],
        )
    };

    let mut cost_first = records.clone();
    cost_first.push(transfer("2026-01-03"));
    assert!(error(cost_first).contains("costed"));

    // Put the plain posting before the first acquisition on the very same day.
    records.insert(0, transfer("2026-01-01"));
    assert!(error(records).contains("costed"));
}

#[test]
fn a_sale_reversal_restores_units_and_offsets_realized_gains() {
    let ledger = Ledger::build(investment_export_records()).unwrap();
    let holdings = report(&ledger, "holdings");
    assert_eq!(holdings[0]["units"], "2.5");
    assert_eq!(holdings[1]["units"], "2");
    let gains = report(&ledger, "gains");
    assert_eq!(
        gains
            .iter()
            .map(|gain| gain["number"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["12.5", "1.5", "-12.5", "-1.5", "3"],
    );
    let export = ledger.export();
    assert!(export.contains(r#""A\"B\\C" "Cost in \"USD\"""#));
    assert!(export.contains(r#"ACME {10 USD, 2026-01-01, "lot-buy-first-1"} @ 15 USD"#));
    assert!(export.contains("2026-01-03 price ACME 15 USD"));
}

fn investment_export_records() -> Vec<Record> {
    let mut records = investments("FIFO");
    records.push(sale("sale", "2026-01-03", "-3", "45", "-14", None));
    records.push(record(
        "reversal",
        json!({ "type": "reverse", "date": "2026-01-04", "target": "sale" }),
    ));
    records.push(sale(
        "explicit",
        "2026-01-05",
        "-1",
        "15",
        "-3",
        Some("lot-buy-second-1"),
    ));
    records.push(record(
        "price",
        json!({
            "type": "price", "date": "2026-01-03",
            "currency": "ACME", "amount": amount("15", "USD")
        }),
    ));
    records
}

#[test]
#[ignore = "requires the upstream bean-check command on PATH"]
fn beancount_accepts_investment_export() {
    let ledger = Ledger::build(investment_export_records()).unwrap();
    ledger.validate_export().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let export_path = directory.path().join("investments.beancount");
    fs::write(&export_path, ledger.export()).unwrap();
    let result = Command::new("bean-check")
        .arg(export_path)
        .output()
        .expect("run Beancount 3 bean-check");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn a_late_entry_before_a_close_is_reversed_on_or_before_the_close() {
    let records = |reversed: &str| {
        vec![
            open("Liabilities:Visa-1234", "SGD", "STRICT"),
            open("Expenses:Transport", "SGD", "STRICT"),
            record(
                "close",
                json!({ "type": "close", "date": "2026-02-01", "account": "Liabilities:Visa-1234" }),
            ),
            // Imported after the close, dated before it.
            transaction(
                "grab",
                "2026-01-20",
                vec![
                    posting("Liabilities:Visa-1234", "-12.50", "SGD"),
                    posting("Expenses:Transport", "12.50", "SGD"),
                ],
            ),
            record(
                "undo",
                json!({ "type": "reverse", "date": reversed, "target": "grab" }),
            ),
        ]
    };
    let ledger = Ledger::build(records("2026-02-01")).unwrap();
    let visa = ledger
        .accounts()
        .into_iter()
        .find(|account| account.name == "Liabilities:Visa-1234")
        .unwrap();
    assert_eq!(visa.closed, "2026-02-01".parse().ok());
    // After the close, the card can no longer take the reversal.
    assert!(error(records("2026-02-02")).contains("closed"));
}
