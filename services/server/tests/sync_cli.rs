//! Live integration test for an empty, disposable self-hosted libSQL database.
//! Set PEANUT_TEST_LIBSQL_URL and run:
//! cargo test --test sync_cli -- --ignored

use std::{
    io::Write,
    path::Path,
    process::{Child, Command, Output, Stdio},
};

use serde_json::{Value, json};

fn peanut(db: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_peanut-count"));
    command.args(["--db"]).arg(db);
    command
}

fn checked(output: Output) -> String {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn add(db: &Path, entry: Value) -> String {
    add_with_id(db, entry, None)
}

fn add_with_id(db: &Path, entry: Value, id: Option<&str>) -> String {
    let mut command = peanut(db);
    command.arg("add");
    if let Some(id) = id {
        command.args(["--id", id]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(entry.to_string().as_bytes())
        .unwrap();
    checked(child.wait_with_output().unwrap()).trim().to_owned()
}

fn start_sync(db: &Path, url: &str) -> Child {
    peanut(db)
        .args(["sync", "--url", url])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn sync(db: &Path, url: &str) {
    checked(start_sync(db, url).wait_with_output().unwrap());
}

fn inspect(db: &Path, args: &[&str]) -> Value {
    let result = checked(peanut(db).args(args).output().unwrap());
    serde_json::from_str(&result).unwrap()
}

fn amount(number: &str, currency: &str) -> Value {
    json!({"number": number, "currency": currency})
}

fn sell(db: &Path, narration: &str, units: &str, cash: &str, gain: &str, lot: &str) -> String {
    add(
        db,
        json!({
            "type": "transaction", "date": "2026-02-02", "narration": narration,
            "postings": [
                {
                    "account": "Assets:Stock",
                    "units": amount(units, "HOLD"),
                    "price": amount("15", "USD"),
                    "sell": true,
                    "lot": lot
                },
                {"account": "Assets:Cash", "units": amount(cash, "USD")},
                {"account": "Income:Gains", "units": amount(gain, "USD")}
            ]
        }),
    )
}

#[test]
#[ignore = "requires an empty disposable sqld database via PEANUT_TEST_LIBSQL_URL"]
fn replicas_converge_after_a_conflicting_offline_sale() {
    let url = std::env::var("PEANUT_TEST_LIBSQL_URL").expect("set PEANUT_TEST_LIBSQL_URL");
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.db");
    let b = dir.path().join("b.db");

    sync(&a, &url);
    assert_eq!(
        inspect(&a, &["status"])["remote_sequence"],
        0,
        "the test requires an empty libSQL ledger"
    );

    for (account, currency) in [
        ("Assets:Cash", "USD"),
        ("Assets:Stock", "HOLD"),
        ("Income:Gains", "USD"),
    ] {
        add(
            &a,
            json!({
                "type": "open", "date": "2026-01-01",
                "account": account, "currencies": [currency]
            }),
        );
    }
    add(
        &a,
        json!({
            "type": "transaction", "date": "2026-02-01", "narration": "Initial purchase",
            "postings": [
                {"account": "Assets:Stock", "units": amount("10", "HOLD"), "cost": amount("10", "USD")},
                {"account": "Assets:Cash", "units": amount("-100", "USD")}
            ]
        }),
    );
    // Replaying a local snapshot from before push simulates loss of the push acknowledgment.
    let retry = dir.path().join("retry.db");
    std::fs::copy(&a, &retry).unwrap();
    sync(&a, &url);
    sync(&retry, &url);
    sync(&a, &url);
    sync(&b, &url);
    assert_eq!(inspect(&retry, &["status"]), inspect(&a, &["status"]));
    let holdings = inspect(&b, &["report", "holdings"]);
    assert_eq!(holdings[0]["units"], "10");
    let lot = holdings[0]["lot"].as_str().unwrap();
    assert_eq!(inspect(&a, &["status"])["remote_sequence"], 4);
    assert_eq!(inspect(&a, &["status"])["pending"], json!([]));

    let a_sale = sell(&a, "Sale from A", "-6", "90", "-30", lot);
    let b_sale = sell(&b, "Sale from B", "-6", "90", "-30", lot);
    assert_ne!(a_sale, b_sale);
    assert_eq!(inspect(&a, &["report", "holdings"])[0]["units"], "4");
    assert_eq!(inspect(&b, &["report", "holdings"])[0]["units"], "4");

    let a_sync = start_sync(&a, &url);
    let b_sync = start_sync(&b, &url);
    let a_result = a_sync.wait_with_output().unwrap();
    let b_result = b_sync.wait_with_output().unwrap();
    let (winner, loser, accepted_id, rejected_id, rejection) =
        match (a_result.status.success(), b_result.status.success()) {
            (true, false) => (&a, &b, a_sale, b_sale, b_result),
            (false, true) => (&b, &a, b_sale, a_sale, a_result),
            _ => panic!("expected one successful sync; A={a_result:?}, B={b_result:?}"),
        };
    assert!(
        String::from_utf8_lossy(&rejection.stderr).contains("insufficient"),
        "{rejection:?}"
    );
    let status = inspect(loser, &["status"]);
    assert_eq!(status["remote_sequence"], 5);
    assert_eq!(status["pending"].as_array().unwrap().len(), 1);
    assert_eq!(status["pending"][0]["id"], rejected_id);
    assert!(
        status["pending"][0]["conflict"]
            .as_str()
            .unwrap()
            .contains("insufficient")
    );

    let register = inspect(winner, &["report", "transactions"]);
    assert_eq!(register.as_array().unwrap().len(), 2);
    assert_eq!(register[1]["id"], accepted_id);
    assert_eq!(inspect(loser, &["report", "transactions"]), register);

    let retry = start_sync(loser, &url).wait_with_output().unwrap();
    assert!(!retry.status.success());
    assert_eq!(inspect(loser, &["status"]), status);

    checked(
        peanut(loser)
            .args(["discard-pending", &rejected_id])
            .output()
            .unwrap(),
    );
    sell(loser, "Replacement sale", "-4", "60", "-20", lot);
    sync(loser, &url);
    sync(winner, &url);
    sync(loser, &url);

    assert_eq!(inspect(winner, &["status"]), inspect(loser, &["status"]));
    assert_eq!(inspect(winner, &["status"])["remote_sequence"], 6);
    assert_eq!(inspect(loser, &["status"])["pending"], json!([]));
    assert_eq!(inspect(winner, &["report", "holdings"]), json!([]));
    assert_eq!(inspect(loser, &["report", "holdings"]), json!([]));
    assert_eq!(
        inspect(winner, &["report", "transactions"]),
        inspect(loser, &["report", "transactions"])
    );

    // Reusing an explicit UUID for different requests must not permanently block pull.
    let shared_id = "671d38c4-0271-4c37-993a-a6596ffe638d";
    let first = json!({"type":"commodity", "date":"2026-02-03", "currency":"FIRST"});
    let second = json!({"type":"commodity", "date":"2026-02-03", "currency":"SECOND"});
    assert_eq!(
        add_with_id(winner, first.clone(), Some(shared_id)),
        shared_id
    );
    assert_eq!(
        add_with_id(winner, first.clone(), Some(shared_id)),
        shared_id
    );
    assert_eq!(add_with_id(loser, second, Some(shared_id)), shared_id);
    sync(winner, &url);
    let rejection = start_sync(loser, &url).wait_with_output().unwrap();
    assert!(!rejection.status.success());
    assert!(String::from_utf8_lossy(&rejection.stderr).contains("same ID"));
    checked(
        peanut(loser)
            .args(["discard-pending", shared_id])
            .output()
            .unwrap(),
    );
    sync(loser, &url);
    assert_eq!(inspect(winner, &["status"]), inspect(loser, &["status"]));
    assert_eq!(inspect(loser, &["status"])["remote_sequence"], 7);
    assert_eq!(add_with_id(loser, first, Some(shared_id)), shared_id);
}
