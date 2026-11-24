//! Turns a forwarded transaction email into a one-row statement.
//!
//! Deterministic code finds every amount, date, and payee phrase written in
//! the email. A decision model then chooses among those candidates and the
//! ledger's accounts, so it never invents a value that the email does not
//! contain.

use std::{collections::BTreeMap, sync::LazyLock};

use anyhow::{Context, Result};
use chrono::{Datelike, Days, NaiveDate};
use mail_parser::{Message, MessageParser, decoders::html::html_to_text};
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{
    jev::{Answer, Answers, Decide},
    mobile::{Statement, StatementRow},
};

/// Longest email text, in characters, that is searched and sent to the model.
const MAX_TEXT: usize = 12_000;
const MAX_CANDIDATES: usize = 12;
/// The model's minimum P(yes) that the email reports a transaction.
const MIN_TRANSACTION: f64 = 0.5;
/// Below this probability the counter-account falls back to Uncategorized.
const MIN_CATEGORY: f64 = 0.3;

#[derive(Debug)]
pub struct Email {
    /// Statement fingerprint derived from the original Message-ID.
    pub id: String,
    pub from: String,
    pub subject: String,
    pub sent: Option<NaiveDate>,
    pub body: String,
}

#[derive(Debug)]
pub enum Failure {
    /// The email cannot become a transaction. Retrying will not help.
    Rejected(String),
    /// The decision model failed. Retrying may help.
    Unavailable(anyhow::Error),
}

fn rejected(reason: impl Into<String>) -> Failure {
    Failure::Rejected(reason.into())
}

pub fn parse(raw: &[u8]) -> Result<Email> {
    let outer = MessageParser::default()
        .parse(raw)
        .context("input is not an email message")?;
    // A forward "as attachment" carries the original alert as message/rfc822.
    let message = outer
        .attachments()
        .find_map(|part| part.message())
        .unwrap_or(&outer);
    let id = match message.message_id().or(outer.message_id()) {
        Some(message_id) => fingerprint(&["peanut-email", message_id.trim()]),
        None => format!("{:x}", Sha256::digest(raw)),
    };
    let from = message
        .from()
        .and_then(|address| address.first())
        .map(|addr| {
            let address = addr.address().unwrap_or_default();
            match addr.name() {
                Some(name) => format!("{name} <{address}>"),
                None => address.to_owned(),
            }
        })
        .unwrap_or_default();
    let sent = message.date().and_then(|date| {
        NaiveDate::from_ymd_opt(date.year.into(), date.month.into(), date.day.into())
    });
    Ok(Email {
        id,
        from: clean(&from),
        subject: strip_forward_prefix(&clean(message.subject().unwrap_or_default())).to_owned(),
        sent,
        body: truncate(&clean(&body_text(message)), MAX_TEXT).to_owned(),
    })
}

static BLOCK_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<(/?(?:td|th|tr|p|div|br|li|h[1-6]|table|section|header|footer)\b)").unwrap()
});

/// The first text body. HTML gets a line break at each cell and block tag so
/// that table cells such as "Total" and "S$17.10" do not run together.
fn body_text(message: &Message) -> String {
    match message.text_part(0) {
        Some(part) if part.is_text_html() => {
            html_to_text(&BLOCK_TAG.replace_all(part.text_contents().unwrap_or_default(), "\n<$1"))
        }
        _ => message.body_text(0).unwrap_or_default().into_owned(),
    }
}

fn fingerprint(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_lines = 0;
    for line in text.lines() {
        let line = line
            .replace(['\u{a0}', '\t'], " ")
            .replace(['\u{200b}', '\u{200c}', '\u{200d}', '\u{feff}'], "");
        let line = line
            .split(' ')
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if line.is_empty() {
            blank_lines += 1;
            if blank_lines > 1 {
                continue;
            }
        } else {
            blank_lines = 0;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.trim().to_owned()
}

fn strip_forward_prefix(mut subject: &str) -> &str {
    loop {
        let lower = subject.to_ascii_lowercase();
        let Some(rest) = ["fwd:", "fw:", "re:"]
            .iter()
            .find_map(|prefix| lower.starts_with(prefix).then(|| &subject[prefix.len()..]))
        else {
            return subject;
        };
        subject = rest.trim_start();
    }
}

fn truncate(text: &str, chars: usize) -> &str {
    text.char_indices()
        .nth(chars)
        .map_or(text, |(at, _)| &text[..at])
}

/// Up to 40 characters on each side of a match, on one line.
fn context(text: &str, start: usize, end: usize) -> String {
    let before: String = text[..start]
        .chars()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let after: String = text[end..].chars().take(40).collect();
    format!("{before}{}{after}", &text[start..end])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Money {
    /// Unsigned decimal without grouping separators.
    pub number: String,
    /// `None` for a bare `$`, which several currencies share.
    pub currency: Option<String>,
}

const CODES: &str = "USD|SGD|EUR|GBP|JPY|AUD|CAD|HKD|NZD|CHF|CNY|RMB|MYR|INR|IDR|THB|PHP|KRW|TWD|VND|SEK|NOK|DKK|MXN|BRL|ZAR|AED";
const NUMBER: &str = r"\d{1,3}(?:,\d{3})+(?:\.\d{1,2})?|\d+(?:\.\d{1,2})?";

static AMOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?:(?P<symbol>US\$|S\$|A\$|C\$|HK\$|NZ\$|\$|€|£|¥|₹)|\b(?P<code>{CODES}))\s?-?(?P<number>{NUMBER})|\b(?P<number2>{NUMBER})\s?(?P<code2>{CODES})\b"
    ))
    .unwrap()
});

/// Every distinct amount written with a currency symbol or ISO code.
pub fn amounts(text: &str) -> Vec<(Money, String)> {
    let mut found: Vec<(Money, String)> = Vec::new();
    for caps in AMOUNT.captures_iter(text) {
        let whole = caps.get(0).unwrap();
        // Skip a match that stops inside a longer number, such as "12.505".
        if text[whole.end()..].starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let number = caps
            .name("number")
            .or(caps.name("number2"))
            .unwrap()
            .as_str()
            .replace(',', "");
        let number = match number.trim_start_matches('0') {
            rest if rest.is_empty() || rest.starts_with('.') => format!("0{rest}"),
            rest => rest.to_owned(),
        };
        if number.len() > 18 || !number.bytes().any(|b| matches!(b, b'1'..=b'9')) {
            continue;
        }
        let currency = match caps.name("symbol").map(|m| m.as_str()) {
            Some("US$") => Some("USD"),
            Some("S$") => Some("SGD"),
            Some("A$") => Some("AUD"),
            Some("C$") => Some("CAD"),
            Some("HK$") => Some("HKD"),
            Some("NZ$") => Some("NZD"),
            Some("€") => Some("EUR"),
            Some("£") => Some("GBP"),
            Some("¥") => Some("JPY"),
            Some("₹") => Some("INR"),
            Some(_) => None,
            None => caps
                .name("code")
                .or(caps.name("code2"))
                .map(|m| match m.as_str() {
                    "RMB" => "CNY",
                    code => code,
                }),
        };
        let money = Money {
            number,
            currency: currency.map(str::to_owned),
        };
        if found.len() < MAX_CANDIDATES && !found.iter().any(|(seen, _)| *seen == money) {
            let label = format!(
                "{}, in “{}”",
                whole.as_str(),
                context(text, whole.start(), whole.end())
            );
            found.push((money, label));
        }
    }
    found
}

const MONTH: &str = r"(?P<month>(?i:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-zA-Z]*)\.?";

static DATES: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        Regex::new(r"\b(?P<year>\d{4})-(?P<m>\d{1,2})-(?P<day>\d{1,2})\b").unwrap(),
        Regex::new(&format!(
            r"\b(?P<day>\d{{1,2}})(?:st|nd|rd|th)?[ -]{MONTH}(?:[ ,-]+(?P<year>\d{{4}}))?\b"
        ))
        .unwrap(),
        Regex::new(&format!(
            r"\b{MONTH} (?P<day>\d{{1,2}})(?:st|nd|rd|th)?(?:,? (?P<year>\d{{4}}))?\b"
        ))
        .unwrap(),
        Regex::new(r"\b(?P<a>\d{1,2})[/.](?P<b>\d{1,2})[/.](?P<year>\d{4}|\d{2})\b").unwrap(),
    ]
});

/// Every distinct plausible date written in the text, within about a year
/// before `anchor`. Slash dates yield both day-first and month-first readings.
pub fn dates(text: &str, anchor: NaiveDate) -> Vec<(NaiveDate, String)> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let earliest = anchor - Days::new(400);
    let latest = anchor + Days::new(1);
    let mut found: Vec<(NaiveDate, String)> = Vec::new();
    for (kind, pattern) in DATES.iter().enumerate() {
        for caps in pattern.captures_iter(text) {
            let whole = caps.get(0).unwrap();
            let number = |name| caps.name(name).and_then(|m| m.as_str().parse::<u32>().ok());
            let year = number("year").map(|year| if year < 100 { year + 2000 } else { year });
            let readings: Vec<(Option<u32>, u32, u32)> = match kind {
                0 => vec![(year, number("m").unwrap(), number("day").unwrap())],
                1 | 2 => {
                    // Accept "Nov", "Sept", or "November", but not "market".
                    let word = caps.name("month").unwrap().as_str().to_ascii_lowercase();
                    let Some(month) = MONTHS.iter().position(|m| m.starts_with(&word)) else {
                        continue;
                    };
                    vec![(year, month as u32 + 1, number("day").unwrap())]
                }
                _ => {
                    let (a, b) = (number("a").unwrap(), number("b").unwrap());
                    vec![(year, b, a), (year, a, b)]
                }
            };
            for (year, month, day) in readings {
                let date = match year {
                    Some(year) => NaiveDate::from_ymd_opt(year as i32, month, day),
                    None => NaiveDate::from_ymd_opt(anchor.year(), month, day).map(|date| {
                        if date > latest {
                            date.with_year(anchor.year() - 1).unwrap_or(date)
                        } else {
                            date
                        }
                    }),
                };
                let Some(date) = date.filter(|date| (earliest..=latest).contains(date)) else {
                    continue;
                };
                if found.len() < MAX_CANDIDATES && !found.iter().any(|(seen, _)| *seen == date) {
                    found.push((date, context(text, whole.start(), whole.end())));
                }
            }
        }
    }
    found
}

static PAYEE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)\b(?i:at|to|from|merchant(?: name)?:|payee:|paid to|recipient:|description:|transaction details:)\s+(?P<payee>[A-Z0-9][A-Za-z0-9&'*./ -]{1,60}?)(?:\s+(?i:on|for|via|using|with|ending|at|from|is|has|was|of)\b|[,;:!()\n]|\.\s|\.?$)",
    )
    .unwrap()
});

/// Phrases that name the other party, such as the text after "at" or "to".
pub fn payees(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for caps in PAYEE.captures_iter(text) {
        let payee = caps["payee"].trim().trim_end_matches(['.', '-', '/', ' ']);
        if payee.len() >= 2
            && payee.bytes().any(|b| b.is_ascii_alphabetic())
            && found.len() < MAX_CANDIDATES
            && !found.iter().any(|seen| seen.eq_ignore_ascii_case(payee))
        {
            found.push(payee.to_owned());
        }
    }
    found
}

fn describe_account(name: &str, transfer: bool) -> String {
    let (root, rest) = name.split_once(':').unwrap_or((name, ""));
    let words = rest.replace([':', '-'], " ");
    // Card and account numbers in names, such as "Citi-Rewards-1234", are the best match signal.
    let numbers: Vec<&str> = rest
        .split([':', '-'])
        .filter(|part| part.len() >= 3 && part.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    let ending = match numbers.last() {
        Some(number) => format!(", card or account number ending {number}"),
        None => String::new(),
    };
    match (root, transfer) {
        ("Assets", false) => format!("your bank, cash, or debit card account “{words}”{ending}"),
        ("Liabilities", false) => format!("your credit card or loan “{words}”{ending}"),
        ("Assets", true) => format!(
            "a transfer between the user's own accounts, with “{words}”{ending}, only when `email` names it"
        ),
        ("Liabilities", true) => format!(
            "a payment between the user's own accounts, with the credit card or loan “{words}”{ending}, only when `email` names it"
        ),
        ("Income", _) => format!("income: {}", words.to_lowercase()),
        _ => format!("spending on {}", words.to_lowercase()),
    }
}

const DEFAULT_SOURCES: [(&str, &str); 2] = [
    ("Assets:Checking", "a bank account or debit card"),
    ("Liabilities:CreditCard", "a credit card"),
];

const DEFAULT_CATEGORIES: [(&str, &str); 14] = [
    ("Expenses:Groceries", "supermarkets and groceries"),
    (
        "Expenses:Dining",
        "restaurants, cafes, bars, coffee, and food delivery",
    ),
    (
        "Expenses:Transport",
        "taxis, ride-hailing, public transport, fuel, and parking",
    ),
    (
        "Expenses:Shopping",
        "retail and online shopping, clothing, and electronics",
    ),
    (
        "Expenses:Bills",
        "utilities, phone, internet, insurance, and subscriptions",
    ),
    ("Expenses:Travel", "flights, hotels, and travel bookings"),
    ("Expenses:Health", "doctors, pharmacies, and fitness"),
    (
        "Expenses:Entertainment",
        "movies, games, events, and streaming",
    ),
    ("Expenses:Fees", "bank fees, charges, and interest charged"),
    (
        "Expenses:Transfers",
        "money sent to another person, such as a PayNow, PayPal, or bank transfer to someone else",
    ),
    (
        "Expenses:Uncategorized",
        "spending that fits no other category",
    ),
    ("Income:Salary", "salary, payroll, and wages"),
    ("Income:Interest", "interest and dividends earned"),
    (
        "Income:Uncategorized",
        "money received that fits no other category",
    ),
];

/// Chooses a statement row for `email` against the ledger's open accounts.
pub async fn extract(
    email: &Email,
    accounts: &[(String, Vec<String>)],
    default_currency: &str,
    model: &impl Decide,
) -> Result<Statement, Failure> {
    let text = format!("{}\n{}", email.subject, email.body);
    if text.trim().is_empty() {
        return Err(rejected("The email has no readable text."));
    }
    let amounts = amounts(&text);
    if amounts.is_empty() {
        return Err(rejected(
            "No amount with a currency symbol or code was found.",
        ));
    }
    let anchor = email
        .sent
        .unwrap_or_else(|| chrono::Utc::now().date_naive());
    let mut dates = dates(&text, anchor);
    if let Some(sent) = email.sent
        && !dates.iter().any(|(date, _)| *date == sent)
    {
        dates.push((sent, "the date this email was sent".into()));
    }
    if dates.is_empty() {
        return Err(rejected("No transaction date was found."));
    }
    let mut payees = payees(&text);
    if !email.subject.is_empty()
        && !payees
            .iter()
            .any(|p| p.eq_ignore_ascii_case(&email.subject))
    {
        payees.push(email.subject.clone());
    }

    let mut sources: BTreeMap<String, String> = accounts
        .iter()
        .filter(|(name, _)| name.starts_with("Assets:") || name.starts_with("Liabilities:"))
        .map(|(name, _)| (name.clone(), describe_account(name, false)))
        .collect();
    if sources.is_empty() {
        sources.extend(DEFAULT_SOURCES.map(|(name, text)| (name.to_owned(), text.to_owned())));
    }
    let mut counters: BTreeMap<String, String> = accounts
        .iter()
        .filter(|(name, _)| !name.starts_with("Equity:"))
        .map(|(name, _)| {
            let transfer = name.starts_with("Assets:") || name.starts_with("Liabilities:");
            (name.clone(), describe_account(name, transfer))
        })
        .collect();
    for (name, text) in DEFAULT_CATEGORIES {
        counters
            .entry(name.to_owned())
            .or_insert_with(|| text.to_owned());
    }
    // Decision APIs accept at most 255 options per question.
    counters = counters.into_iter().take(250).collect();
    sources = sources.into_iter().take(250).collect();

    let indexed = |items: Vec<String>| -> Map<String, Value> {
        items
            .into_iter()
            .enumerate()
            .map(|(i, text)| ((i + 1).to_string(), Value::String(text)))
            .collect()
    };
    let mut questions = Map::new();
    questions.insert("transaction".into(), json!({
        "type": "noul",
        "instructions": "Does `email` report one specific payment, purchase, transfer, withdrawal, refund, or deposit that moved a stated amount of money? Promotions, statements, balance summaries, bills not yet paid, one-time passwords, and login alerts do not count.",
    }));
    questions.insert("direction".into(), json!({
        "type": "choice",
        "instructions": "For the user's account that `email` is about, did the money go out or come in?",
        "criteria": {
            "out": "Money left the account or was charged to it: a purchase, payment, transfer out, withdrawal, or fee.",
            "in": "Money arrived in the account or was credited to it: a refund, deposit, salary, interest, or transfer in.",
        },
    }));
    if amounts.len() > 1 {
        questions.insert("amount".into(), json!({
            "type": "choice",
            "instructions": "Which amount in `email` is the full amount of money that moved in the transaction? For a receipt this is the total paid, not a single line item. It is never an account balance or credit limit.",
            "criteria": indexed(amounts.iter().map(|(_, context)| context.clone()).collect()),
        }));
    }
    if dates.len() > 1 {
        questions.insert("date".into(), json!({
            "type": "choice",
            "instructions": "On which date did the transaction in `email` happen?",
            "criteria": indexed(dates.iter().map(|(date, context)| format!("{date}, from “{context}”")).collect()),
        }));
    }
    if !payees.is_empty() {
        let mut criteria = indexed(payees.clone());
        criteria.insert(
            "none".into(),
            "None of these names the merchant or other party.".into(),
        );
        questions.insert("payee".into(), json!({
            "type": "choice",
            "instructions": "Which text names the merchant, person, or organisation on the other side of the transaction in `email`?",
            "criteria": criteria,
        }));
    }
    if sources.len() > 1 {
        questions.insert("account".into(), json!({
            "type": "choice",
            "instructions": "Which of the user's accounts did the transaction in `email` use? Match bank names and card or account numbers where possible.",
            "criteria": sources,
        }));
    }

    let state = json!({
        "email": format!(
            "From: {}\nSubject: {}\nSent: {}\n\n{}",
            email.from,
            email.subject,
            email.sent.map(|date| date.to_string()).unwrap_or_default(),
            email.body,
        ),
    });
    let answers = model
        .decide(state.clone(), Value::Object(questions))
        .await
        .map_err(Failure::Unavailable)?;

    match answers.get("transaction") {
        Some(Answer::Noul { noul }) if *noul >= MIN_TRANSACTION => {}
        Some(Answer::Noul { .. }) => {
            return Err(rejected(
                "The email does not look like a single completed transaction.",
            ));
        }
        _ => {
            return Err(Failure::Unavailable(anyhow::anyhow!(
                "decision model did not answer `transaction`"
            )));
        }
    }
    let outgoing = choice(&answers, "direction", &["out", "in"])? == "out";
    let (money, _) = &amounts[index(&answers, "amount", amounts.len())?];
    let (date, _) = dates[index(&answers, "date", dates.len())?];
    let payee = if payees.is_empty() {
        None
    } else {
        let keys: Vec<String> = (1..=payees.len())
            .map(|i| i.to_string())
            .chain(["none".into()])
            .collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        choice(&answers, "payee", &keys)?
            .parse::<usize>()
            .ok()
            .map(|i| payees[i - 1].clone())
    };
    let source = if sources.len() == 1 {
        sources.keys().next().unwrap().clone()
    } else {
        let keys: Vec<&str> = sources.keys().map(String::as_str).collect();
        choice(&answers, "account", &keys)?.to_owned()
    };

    // Ask for the category separately: in the same call the model tends to
    // pick the source account the email names, which crowds out categories.
    counters.remove(&source);
    let instructions = if outgoing {
        "Money left the user's account in `email`. Which account best describes what it was spent on, or which of the user's own accounts received it?"
    } else {
        "Money arrived in the user's account in `email`. Which account best describes where it came from? For a refund, choose the spending category it reverses. For a transfer, choose the user's own account it came from."
    };
    let category = json!({
        "category": { "type": "choice", "instructions": instructions, "criteria": counters },
    });
    let answers = model
        .decide(state, category)
        .await
        .map_err(Failure::Unavailable)?;
    let account = match answers.get("category") {
        Some(Answer::Choice {
            probabilities,
            choice,
        }) if counters.contains_key(choice) => {
            let p = probabilities.get(choice).copied().unwrap_or(1.0);
            (p >= MIN_CATEGORY).then(|| choice.clone())
        }
        Some(Answer::Refusal) => None,
        _ => {
            return Err(Failure::Unavailable(anyhow::anyhow!(
                "decision model did not answer `category`"
            )));
        }
    }
    .unwrap_or_else(|| {
        if outgoing {
            "Expenses:Uncategorized"
        } else {
            "Income:Uncategorized"
        }
        .to_owned()
    });

    let currency = money.currency.clone().unwrap_or_else(|| {
        match accounts.iter().find(|(name, _)| *name == source) {
            Some((_, currencies)) if currencies.len() == 1 => currencies[0].clone(),
            _ => default_currency.to_owned(),
        }
    });
    let description = payee
        .or_else(|| (!email.subject.is_empty()).then(|| email.subject.clone()))
        .unwrap_or_else(|| "Email transaction".into());
    Ok(Statement {
        id: email.id.clone(),
        source,
        currency,
        rows: vec![StatementRow {
            date,
            description: truncate(&description, 200).to_owned(),
            amount: if outgoing {
                format!("-{}", money.number)
            } else {
                money.number.clone()
            },
            account,
        }],
    })
}

fn choice<'a>(answers: &'a Answers, name: &str, keys: &[&str]) -> Result<&'a str, Failure> {
    match answers.get(name) {
        Some(Answer::Choice { choice, .. }) if keys.contains(&choice.as_str()) => Ok(choice),
        Some(Answer::Refusal) => Err(rejected(format!(
            "The decision model declined to choose the {name}."
        ))),
        _ => Err(Failure::Unavailable(anyhow::anyhow!(
            "decision model gave no valid `{name}` answer"
        ))),
    }
}

/// Zero-based index for a question offered with keys "1"..="len", or 0 when it was not asked.
fn index(answers: &Answers, name: &str, len: usize) -> Result<usize, Failure> {
    if len == 1 {
        return Ok(0);
    }
    let keys: Vec<String> = (1..=len).map(|i| i.to_string()).collect();
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    Ok(choice(answers, name, &keys)?.parse::<usize>().unwrap() - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALERT: &str = "Message-ID: <alert-1@bank.example>\r\n\
From: Bank Alerts <alerts@bank.example>\r\n\
Subject: Fwd: Card transaction alert\r\n\
Date: Tue, 24 Nov 2026 09:15:00 +0800\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
A transaction of S$12.50 was made with your card ending 1234 on 23 Nov 2026 at STARBUCKS VIVOCITY.\r\n\
Available balance: SGD 1,204.33.\r\n";

    /// Returns the same answers to every call and records each call's questions.
    struct Fixed {
        answers: Answers,
        calls: std::sync::Mutex<Vec<Value>>,
    }

    fn fixed(answers: Answers) -> Fixed {
        Fixed {
            answers,
            calls: Default::default(),
        }
    }

    impl Decide for Fixed {
        async fn decide(&self, _: Value, questions: Value) -> Result<Answers> {
            self.calls.lock().unwrap().push(questions);
            Ok(self.answers.clone())
        }
    }

    fn choice(value: &str, probabilities: &[(&str, f64)]) -> Answer {
        Answer::Choice {
            choice: value.into(),
            probabilities: probabilities
                .iter()
                .map(|(k, p)| (k.to_string(), *p))
                .collect(),
        }
    }

    fn date(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    #[test]
    fn finds_amounts_with_currency_markers_only() {
        let found = amounts(
            "Spent S$12.50 and US$3 at A. Balance SGD 1,204.33, ref 123456, 7.00 EUR, $5.25, $0.00",
        );
        let found: Vec<_> = found
            .into_iter()
            .map(|(money, _)| (money.number, money.currency))
            .collect();
        assert_eq!(
            found,
            [
                ("12.50".into(), Some("SGD".into())),
                ("3".into(), Some("USD".into())),
                ("1204.33".into(), Some("SGD".into())),
                ("7.00".into(), Some("EUR".into())),
                ("5.25".into(), None),
            ]
        );
    }

    #[test]
    fn finds_plausible_dates() {
        let anchor = date("2026-11-24");
        let found: Vec<_> = dates(
            "on 23 Nov 2026, Nov 22, 3rd November, 2026-11-20, 05/11/2026, at the market 12, born 1 Jan 1990, Sept 30",
            anchor,
        )
        .into_iter()
        .map(|(date, _)| date.to_string())
        .collect();
        assert_eq!(
            found,
            [
                "2026-11-20",
                "2026-11-23",
                "2026-11-03",
                "2026-11-22",
                "2026-09-30",
                "2026-11-05",
                "2026-05-11"
            ]
        );
        // A yearless date after the anchor belongs to the previous year.
        assert_eq!(dates("on 30 Dec", anchor)[0].0, date("2025-12-30"));
    }

    #[test]
    fn finds_payee_phrases() {
        assert_eq!(
            payees(
                "made on 23 Nov at STARBUCKS VIVOCITY on 23 Nov.\nMerchant name: Grab Pte Ltd\nat 10:15 to your account"
            ),
            ["STARBUCKS VIVOCITY", "Grab Pte Ltd"]
        );
    }

    #[test]
    fn parses_inline_and_attached_forwards() {
        let email = parse(ALERT.as_bytes()).unwrap();
        assert_eq!(email.subject, "Card transaction alert");
        assert_eq!(email.sent, Some(date("2026-11-24")));
        assert_eq!(email.from, "Bank Alerts <alerts@bank.example>");
        assert_eq!(
            email.id,
            fingerprint(&["peanut-email", "alert-1@bank.example"])
        );

        let forwarded = format!(
            "Message-ID: <fwd-9@mail.example>\r\nSubject: Fwd: alert\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nFYI\r\n\
--b\r\nContent-Type: message/rfc822\r\n\r\n{ALERT}\r\n--b--\r\n"
        );
        let inner = parse(forwarded.as_bytes()).unwrap();
        assert_eq!(
            inner.id, email.id,
            "re-forwarding the same alert deduplicates"
        );
        assert!(inner.body.contains("STARBUCKS VIVOCITY"));
    }

    #[tokio::test]
    async fn builds_a_signed_row_from_model_choices() {
        let email = parse(ALERT.as_bytes()).unwrap();
        let accounts = vec![
            ("Assets:DBS-Savings".to_string(), vec!["SGD".to_string()]),
            ("Liabilities:DBS-Card-1234".to_string(), vec![]),
            ("Expenses:Coffee".to_string(), vec![]),
        ];
        let answers = Answers::from([
            ("transaction".into(), Answer::Noul { noul: 0.97 }),
            ("direction".into(), choice("out", &[])),
            ("amount".into(), choice("1", &[])),
            ("date".into(), choice("1", &[])),
            ("payee".into(), choice("1", &[])),
            ("account".into(), choice("Liabilities:DBS-Card-1234", &[])),
            (
                "category".into(),
                choice(
                    "Expenses:Coffee",
                    &[("Expenses:Coffee", 0.45), ("Expenses:Dining", 0.05)],
                ),
            ),
        ]);
        let model = fixed(answers.clone());
        let statement = extract(&email, &accounts, "USD", &model).await.unwrap();
        // The category is asked on its own, without the chosen source account.
        let calls = model.calls.into_inner().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].get("category").is_none() && calls[0].get("account").is_some());
        let options = calls[1]["category"]["criteria"].as_object().unwrap();
        assert!(
            options.contains_key("Assets:DBS-Savings-8890")
                || options.contains_key("Assets:DBS-Savings")
        );
        assert!(!options.contains_key("Liabilities:DBS-Card-1234"));
        assert_eq!(statement.source, "Liabilities:DBS-Card-1234");
        assert_eq!(statement.currency, "SGD");
        let row = &statement.rows[0];
        assert_eq!(
            (
                row.date,
                row.description.as_str(),
                row.amount.as_str(),
                row.account.as_str()
            ),
            (
                date("2026-11-23"),
                "STARBUCKS VIVOCITY",
                "-12.50",
                "Expenses:Coffee"
            )
        );
        statement.entries().unwrap();

        let mut unsure = answers.clone();
        unsure.insert("direction".into(), choice("in", &[]));
        unsure.insert(
            "category".into(),
            choice(
                "Expenses:Coffee",
                &[("Expenses:Coffee", 0.2), ("Income:Salary", 0.19)],
            ),
        );
        let row = extract(&email, &accounts, "USD", &fixed(unsure))
            .await
            .unwrap()
            .rows
            .remove(0);
        assert_eq!(
            (row.amount.as_str(), row.account.as_str()),
            ("12.50", "Income:Uncategorized")
        );

        let mut promo = answers;
        promo.insert("transaction".into(), Answer::Noul { noul: 0.1 });
        assert!(matches!(
            extract(&email, &accounts, "USD", &fixed(promo)).await,
            Err(Failure::Rejected(_))
        ));
    }

    #[tokio::test]
    async fn bare_dollar_uses_the_account_currency_then_the_default() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        email.subject = String::new();
        email.body = "You paid $8.00 to Grab on 2026-11-23.".into();
        let answers = Answers::from([
            ("transaction".into(), Answer::Noul { noul: 0.9 }),
            ("direction".into(), choice("out", &[])),
            ("date".into(), choice("1", &[])),
            ("payee".into(), choice("1", &[])),
            (
                "category".into(),
                choice("Expenses:Transport", &[("Expenses:Transport", 0.9)]),
            ),
        ]);
        let only = vec![("Assets:Wallet".to_string(), vec!["SGD".to_string()])];
        let statement = extract(&email, &only, "USD", &fixed(answers.clone()))
            .await
            .unwrap();
        assert_eq!(
            (statement.source.as_str(), statement.currency.as_str()),
            ("Assets:Wallet", "SGD")
        );
        assert_eq!(statement.rows[0].description, "Grab");

        let mut answers = answers;
        answers.insert("account".into(), choice("Assets:Checking", &[]));
        let statement = extract(&email, &[], "AUD", &fixed(answers)).await.unwrap();
        assert_eq!(
            (statement.source.as_str(), statement.currency.as_str()),
            ("Assets:Checking", "AUD")
        );
    }
}
