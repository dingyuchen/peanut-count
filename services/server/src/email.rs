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
    ledger::OpenAccount,
    mobile::{Statement, StatementRow},
};

/// Longest email text, in characters, that is searched and sent to the model.
const MAX_TEXT: usize = 12_000;
const MAX_CANDIDATES: usize = 12;
/// Longest subject or sender, in characters.
const MAX_HEADER: usize = 300;
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
        from: clean(truncate(&from, MAX_HEADER)),
        subject: strip_forward_prefix(&clean(truncate(
            message.subject().unwrap_or_default(),
            MAX_HEADER,
        )))
        .to_owned(),
        sent,
        body: truncate(&join_split_amounts(&clean(&body_text(message))), MAX_TEXT).to_owned(),
    })
}

static BLOCK_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<(/?(?:tr|p|div|br|li|h[1-6]|table|section|header|footer)\b)").unwrap()
});

static CELL_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<(/?t[dh]\b)").unwrap());

/// The first text body. HTML gets a line break at each row and block tag and
/// a space between table cells, so "Total" and "S$17.10" do not run together
/// while "SGD" and "12.50" in one row stay on one line.
fn body_text(message: &Message) -> String {
    match message.text_part(0) {
        Some(part) if part.is_text_html() => {
            // html_to_text collapses whitespace, so mark the breaks with
            // private-use characters and restore them afterwards.
            let html = part.text_contents().unwrap_or_default();
            let html = BLOCK_TAG.replace_all(html, "\u{e000}<$1");
            let html = CELL_TAG.replace_all(&html, "\u{e001}<$1");
            html_to_text(&html)
                .replace('\u{e000}', "\n")
                .replace('\u{e001}', " ")
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
    while let Some(prefix) = ["fwd:", "fw:", "re:"].iter().find(|prefix| {
        subject
            .get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    }) {
        subject = subject[prefix.len()..].trim_start();
    }
    subject
}

/// The longest prefix of `text` within `bytes`, ending on a character boundary.
fn truncate_bytes(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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
const NUMBER: &str = r"[0-9]{1,3}(?:,[0-9]{3})+(?:\.[0-9]{1,2})?|[0-9]+(?:\.[0-9]{1,2})?";

static CODE_BEFORE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"\b({CODES}) ?$")).unwrap());

static CODE_AFTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^ ?({CODES})\b")).unwrap());

const SYMBOLS: &str = r"US\$|CA\$|AU\$|S\$|A\$|C\$|HK\$|NZ\$|\$|€|£|¥|₹";

/// A currency symbol or code followed by a number, as in "S$12.50" or "SGD 12.50".
static PREFIX_AMOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?:(?P<symbol>{SYMBOLS})|\b(?P<code>{CODES})) ?-?(?P<number>{NUMBER})"
    ))
    .unwrap()
});

/// A number followed by a currency code, as in "12.50 SGD". Matched separately
/// from [`PREFIX_AMOUNT`] so that in "Ref 123 SGD 12.50" both readings are
/// offered and the model chooses.
static SUFFIX_AMOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"\b(?P<number2>{NUMBER}) ?(?P<code2>{CODES})\b")).unwrap()
});

/// A line holding only a currency marker before a line starting with a number,
/// or a number before a line holding only a code, as table cells often render.
static SPLIT_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?m)^((?:{CODES})|{SYMBOLS})\n+([0-9])")).unwrap());
static SPLIT_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?m)^((?:(?:{SYMBOLS}) ?)?[0-9][0-9,.]*)\n+((?:{CODES})$)"
    ))
    .unwrap()
});

/// Joins a currency marker and its number that layout split across lines.
fn join_split_amounts(text: &str) -> String {
    let text = SPLIT_PREFIX.replace_all(text, "$1 $2");
    SPLIT_SUFFIX.replace_all(&text, "$1 $2").into_owned()
}

/// Whether `number` is only part of a longer or differently formatted number,
/// such as "12.505", "€12,34", "€12.٣٤", the "56" in "1.234,56 EUR", or the
/// "1" in "EUR 1 234.56". A space before an unrelated number, as in
/// "SGD 12.50 23 Nov", does not count.
fn partial_number(before: &str, number: &str, after: &str) -> bool {
    // Any Unicode digit next to the match continues the number.
    let digit = |c: Option<char>| c.is_some_and(char::is_numeric);
    let decimal_mark = |c: Option<char>| matches!(c, Some(',' | '.'));
    let group_space = |c: Option<char>| {
        matches!(
            c,
            Some(' ' | '\'' | '’' | '\u{a0}' | '\u{202f}' | '\u{2009}')
        )
    };
    // Exactly three digits, not followed by another digit.
    let group = |s: &str| {
        let mut chars = s.chars();
        chars.by_ref().take(3).filter(char::is_ascii_digit).count() == 3 && !digit(chars.next())
    };
    let (next, next2) = (after.chars().next(), after.chars().nth(1));
    let (prev, prev2) = (before.chars().next_back(), before.chars().rev().nth(1));
    let whole = number.split('.').next().unwrap_or_default();
    digit(next)
        || digit(prev)
        || (decimal_mark(next) && digit(next2))
        || (decimal_mark(prev) && digit(prev2))
        || (group_space(next)
            && !number.contains(['.', ','])
            && number.len() <= 3
            && group(&after[next.map_or(0, char::len_utf8)..]))
        || (group_space(prev) && digit(prev2) && whole.len() == 3 && !whole.contains(','))
}

/// Every distinct amount written with a currency symbol or ISO code.
pub fn amounts(text: &str) -> Vec<(Money, String)> {
    let mut found: Vec<(Money, String, u8)> = Vec::new();
    let mut matches: Vec<_> = PREFIX_AMOUNT
        .captures_iter(text)
        .chain(SUFFIX_AMOUNT.captures_iter(text))
        .collect();
    matches.sort_by_key(|caps| caps.get(0).unwrap().start());
    for caps in matches {
        let whole = caps.get(0).unwrap();
        let span = caps.name("number").or(caps.name("number2")).unwrap();
        let after = &text[span.end()..];
        let before = &text[..span.start()];
        // A symbol before the number makes it a prefix amount, already handled.
        if caps.name("number2").is_some()
            && before
                .trim_end_matches(' ')
                .ends_with(['$', '€', '£', '¥', '₹'])
        {
            continue;
        }
        if partial_number(before, span.as_str(), after) {
            continue;
        }
        let number = span.as_str().replace(',', "");
        let number = match number.trim_start_matches('0') {
            rest if rest.is_empty() || rest.starts_with('.') => format!("0{rest}"),
            rest => rest.to_owned(),
        };
        if number.len() > 18 || !number.bytes().any(|b| matches!(b, b'1'..=b'9')) {
            continue;
        }
        // An ISO code next to a "$" or "¥" amount, as in "CAD $25.00" or
        // "$25.00 CAD", decides the currency.
        let leading_code = caps.name("symbol").and_then(|symbol| {
            CODE_BEFORE
                .captures(&text[..symbol.start()])
                .map(|caps| caps.get(1).unwrap().as_str())
        });
        let trailing_code = caps.name("symbol").and(
            CODE_AFTER
                .captures(after)
                .map(|caps| caps.get(1).unwrap().as_str()),
        );
        // Codes on both sides that disagree, as in "CAD $25.00 USD", are ambiguous.
        if let (Some(a), Some(b)) = (leading_code, trailing_code)
            && a != b
        {
            continue;
        }
        let code = leading_code.or(trailing_code);
        // Otherwise a symbol glued to letters, such as the "$" in "R$" or
        // "NT$", names a currency this parser does not know.
        if let Some(symbol) = caps.name("symbol")
            && leading_code.is_none()
            && text[..symbol.start()].ends_with(|c: char| c.is_alphabetic())
        {
            continue;
        }
        let currency = match caps.name("symbol").map(|m| m.as_str()) {
            Some("$" | "¥") if code.is_some() => code,
            Some("US$") => Some("USD"),
            Some("CA$") => Some("CAD"),
            Some("AU$") => Some("AUD"),
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
        }
        .map(|code| if code == "RMB" { "CNY" } else { code });
        let money = Money {
            number,
            currency: currency.map(str::to_owned),
        };
        let label = format!(
            "{}, in “{}”",
            whole.as_str(),
            context(text, whole.start(), whole.end())
        );
        // The label is the text on the same line just before the amount, or
        // the previous non-blank line when the amount starts its line, as in
        // "Total paid" above "$143.27".
        let before_amount = &text[..whole.start()];
        let line = &before_amount[before_amount.rfind('\n').map_or(0, |at| at + 1)..];
        let label_line = if line.trim().is_empty() {
            before_amount
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or_default()
        } else {
            line
        };
        let tier = payment_tier(label_line);
        match found.iter_mut().position(|(seen, _, _)| *seen == money) {
            // Keep the strongest label for a repeated amount.
            Some(at) if tier < found[at].2 => found[at] = (money, label, tier),
            Some(_) => {}
            None => found.push((money, label, tier)),
        }
    }
    // An itemised receipt can list more prices than the model is offered.
    // Keep payment amounts and grand totals ahead of line items.
    if found.len() > MAX_CANDIDATES {
        let mut ranked: Vec<_> = found.into_iter().enumerate().collect();
        ranked.sort_by_key(|(i, (_, _, tier))| (*tier, *i));
        ranked.truncate(MAX_CANDIDATES);
        ranked.sort_by_key(|(i, _)| *i);
        found = ranked.into_iter().map(|(_, item)| item).collect();
    }
    found
        .into_iter()
        .map(|(money, label, _)| (money, label))
        .collect()
}

/// How strongly a label marks the amount after it as the payment: 0 for a
/// payment or grand total, 1 for another total or amount, 2 for the rest,
/// including line totals and subtotals.
fn payment_tier(label: &str) -> u8 {
    let label = label.to_lowercase();
    let tail: String = label
        .chars()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let has = |words: &[&str]| words.iter().any(|word| tail.contains(word));
    if has(&[
        "subtotal",
        "sub-total",
        "line total",
        "item total",
        "unit price",
    ]) {
        2
    } else if has(&[
        "grand total",
        "total paid",
        "amount paid",
        "total charged",
        "paid",
        "charged",
        "payment",
        "debited",
        "credited",
        "spent",
        "refund",
        "transaction",
    ]) {
        0
    } else if has(&["total", "amount"]) {
        1
    } else {
        2
    }
}

/// The trailing digits of a card or account number, as in "ending 1234",
/// "ending in 1234", or "XXXX1234".
static CARD_DIGITS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:\bending(?: in| with)?\s*|\bends (?:in|with)\s*|[x*•]{2,}[ -]?)([0-9]{3,4})\b",
    )
    .unwrap()
});

const MONTH: &str = r"(?P<month>(?i:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-zA-Z]*)\.?";

static DATES: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        // Also the date of an ISO timestamp such as 2026-11-23T23:58:00+08:00.
        // Year first, as in 2026-11-23, 2026/11/23, or 2026-11-23T23:58.
        Regex::new(r"\b(?P<year>[0-9]{4})[-/.](?P<m>[0-9]{1,2})[-/.](?P<day>[0-9]{1,2})(?:T|\b)")
            .unwrap(),
        Regex::new(&format!(
            r"\b(?P<day>[0-9]{{1,2}})(?:st|nd|rd|th)?[ -]{MONTH}(?:[ ,-]+(?P<year>[0-9]{{4}}))?\b"
        ))
        .unwrap(),
        Regex::new(&format!(
            r"\b{MONTH} (?P<day>[0-9]{{1,2}})(?:st|nd|rd|th)?(?:,? (?P<year>[0-9]{{4}}))?\b"
        ))
        .unwrap(),
        Regex::new(
            r"\b(?P<a>[0-9]{1,2})(?P<s1>[/.-])(?P<b>[0-9]{1,2})(?P<s2>[/.-])(?P<year>[0-9]{4}|[0-9]{2})\b",
        )
        .unwrap(),
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
                0 => {
                    let (Some(month), Some(day)) = (number("m"), number("day")) else {
                        continue;
                    };
                    vec![(year, month, day)]
                }
                1 | 2 => {
                    // Accept "Nov", "Sept", or "November", but not "market".
                    let word = caps["month"].to_ascii_lowercase();
                    let (Some(month), Some(day)) = (
                        MONTHS.iter().position(|m| m.starts_with(&word)),
                        number("day"),
                    ) else {
                        continue;
                    };
                    vec![(year, month as u32 + 1, day)]
                }
                _ => {
                    let (Some(a), Some(b)) = (number("a"), number("b")) else {
                        continue;
                    };
                    // "11-23/2026" is not a date.
                    if caps["s1"] != caps["s2"] {
                        continue;
                    }
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
    accounts: &[OpenAccount],
    closed: &[String],
    default_currency: &str,
    model: &impl Decide,
) -> Result<Statement, Failure> {
    let is_closed = |name: &str| closed.iter().any(|closed| closed == name);
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
        .map(|account| &account.name)
        .filter(|name| name.starts_with("Assets:") || name.starts_with("Liabilities:"))
        .map(|name| (name.clone(), describe_account(name, false)))
        .collect();
    // The ledger may lack the card or account the email names. Only when the
    // email cites a number, such as "ending 5678", that none of the numbered
    // accounts carries can the model answer "none".
    let cited: Vec<&str> = CARD_DIGITS
        .captures_iter(&text)
        .map(|caps| caps.get(1).unwrap().as_str())
        .collect();
    let numbered: Vec<&String> = sources
        .keys()
        .filter(|name| {
            name.split([':', '-'])
                .any(|part| part.len() >= 3 && part.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    let offer_none = !cited.is_empty()
        && !numbered.is_empty()
        && !cited.iter().any(|number| {
            numbered.iter().any(|name| {
                name.split([':', '-'])
                    .any(|part| part.bytes().all(|b| b.is_ascii_digit()) && part.ends_with(number))
            })
        });
    // Offer each kind the ledger lacks, so the first alert for a new credit
    // card is not booked to the only bank account.
    for (name, text) in DEFAULT_SOURCES {
        let kind = &name[..=name.find(':').unwrap()];
        if !is_closed(name) && !sources.keys().any(|source| source.starts_with(kind)) {
            sources.insert(name.to_owned(), text.to_owned());
        }
    }
    if sources.is_empty() {
        return Err(rejected(
            "The ledger has no open asset or liability account.",
        ));
    }
    let mut counters: BTreeMap<String, String> = accounts
        .iter()
        .map(|account| &account.name)
        .filter(|name| !name.starts_with("Equity:"))
        .map(|name| {
            let transfer = name.starts_with("Assets:") || name.starts_with("Liabilities:");
            (name.clone(), describe_account(name, transfer))
        })
        .collect();
    for (name, text) in DEFAULT_CATEGORIES {
        if !is_closed(name) {
            counters
                .entry(name.to_owned())
                .or_insert_with(|| text.to_owned());
        }
    }
    let sources = limit(sources, &text, |_| 1);

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
    if sources.len() > 1 || offer_none {
        let mut criteria: Map<String, Value> = sources
            .iter()
            .map(|(name, text)| (name.clone(), Value::String(text.clone())))
            .collect();
        if offer_none {
            criteria.insert(
                "none".into(),
                "None of these: `email` names a different card or account.".into(),
            );
        }
        questions.insert("account".into(), json!({
            "type": "choice",
            "instructions": "Which of the user's accounts did the transaction in `email` use? Match bank names and card or account numbers where possible.",
            "criteria": criteria,
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
    let chosen_amount = index(&answers, "amount", amounts.len())?;
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
    let find = |name: &str| accounts.iter().find(|account| account.name == name);
    // Accounts created on import accept any currency.
    let accepts = |name: &str, currency: &str| find(name).is_none_or(|a| a.accepts(currency));
    let source = if sources.len() == 1 && !offer_none {
        sources.keys().next().unwrap().clone()
    } else {
        let keys: Vec<&str> = sources
            .keys()
            .map(String::as_str)
            .chain(offer_none.then_some("none"))
            .collect();
        match choice(&answers, "account", &keys)? {
            "none" => {
                return Err(rejected(
                    "The email names an account that is not in the ledger. Open it, then forward the email again.",
                ));
            }
            source => source.to_owned(),
        }
    };
    // Keep the account the email identifies. If the chosen amount is in a
    // currency it cannot hold, such as a foreign purchase price, ask which
    // amount was settled in a currency the account holds.
    // A bare "$" takes the account's only currency, or the configured default.
    let resolve = |money: &Money| {
        money.currency.clone().unwrap_or_else(|| {
            let account = find(&source).cloned().unwrap_or_default();
            match (&account.declared[..], &account.posted[..]) {
                ([only], _) | ([], [only]) => only.clone(),
                _ => default_currency.to_owned(),
            }
        })
    };
    let fits = |money: &Money| accepts(&source, &resolve(money));
    let (chosen, label) = &amounts[chosen_amount];
    let money = if fits(chosen) {
        chosen.clone()
    } else {
        let compatible: Vec<&(Money, String)> =
            amounts.iter().filter(|(money, _)| fits(money)).collect();
        let held = find(&source)
            .map(|a| a.declared.join(", "))
            .unwrap_or_default();
        let unsettled = || {
            rejected(format!(
                "{source} holds only {held}, and the email has no settled amount in that currency."
            ))
        };
        if compatible.is_empty() {
            return Err(unsettled());
        }
        let mut criteria = indexed(compatible.iter().map(|(_, label)| label.clone()).collect());
        criteria.insert(
            "none".into(),
            "None of these is the amount charged or credited to the account.".into(),
        );
        let question = json!({ "settled": {
            "type": "choice",
            "instructions": format!(
                "The transaction in `email` is for {label}, but the user's account holds only {held}. Which amount is what was actually charged or credited to the account in {held}? An account balance or credit limit is never the answer."
            ),
            "criteria": criteria,
        }});
        let settled = model
            .decide(state.clone(), question)
            .await
            .map_err(Failure::Unavailable)?;
        let keys: Vec<String> = (1..=compatible.len())
            .map(|i| i.to_string())
            .chain(["none".into()])
            .collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        match choice(&settled, "settled", &keys)?.parse::<usize>() {
            Ok(i) => compatible[i - 1].0.clone(),
            Err(_) => return Err(unsettled()),
        }
    };
    let currency = resolve(&money);
    if !accepts(&source, &currency) {
        return Err(rejected(format!("{source} does not accept {currency}.")));
    }

    // Ask for the category separately: in the same call the model tends to
    // pick the source account the email names, which crowds out categories.
    counters.remove(&source);
    counters.retain(|name, _| accepts(name, &currency));
    let counters = limit(counters, &text, |name| {
        let root = name.split(':').next().unwrap_or_default();
        match (root, outgoing) {
            ("Expenses", true) | ("Income", false) => 1,
            ("Expenses", false) => 2,
            ("Income", true) => 4,
            _ => 3,
        }
    });
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
    .map_or_else(
        || {
            let fallback = if outgoing {
                "Expenses:Uncategorized"
            } else {
                "Income:Uncategorized"
            };
            if is_closed(fallback) || !accepts(fallback, &currency) {
                Err(rejected(format!(
                    "No category fits and the fallback {fallback} cannot take {currency}."
                )))
            } else {
                Ok(fallback.to_owned())
            }
        },
        Ok,
    )?;

    let description = payee
        .or_else(|| (!email.subject.is_empty()).then(|| email.subject.clone()))
        .unwrap_or_else(|| "Email transaction".into());
    Ok(Statement {
        id: email.id.clone(),
        source,
        currency,
        rows: vec![StatementRow {
            date,
            // Statement rows allow 512 bytes.
            description: truncate_bytes(truncate(&description, 200), 512).to_owned(),
            amount: if outgoing {
                format!("-{}", money.number)
            } else {
                money.number.clone()
            },
            account,
        }],
    })
}

/// Options some decision backends accept per question; llama.cpp allows 52 for OpenJev.
const MAX_CHOICES: usize = 50;

/// Keeps at most [`MAX_CHOICES`] options: the Uncategorized fallbacks, then
/// accounts whose name parts appear in `text`, then by `rank`, then by name.
fn limit(
    options: BTreeMap<String, String>,
    text: &str,
    rank: impl Fn(&str) -> u8,
) -> BTreeMap<String, String> {
    if options.len() <= MAX_CHOICES {
        return options;
    }
    let cited: Vec<&str> = CARD_DIGITS
        .captures_iter(text)
        .map(|caps| caps.get(1).unwrap().as_str())
        .collect();
    let text = text.to_lowercase();
    // A name part in the email, or a numeric part ending in a cited card number.
    let mentioned = |name: &str| {
        name.split([':', '-']).skip(1).any(|part| {
            (part.len() >= 3 && text.contains(&part.to_lowercase()))
                || (part.bytes().all(|b| b.is_ascii_digit())
                    && cited.iter().any(|number| part.ends_with(number)))
        })
    };
    let mut ordered: Vec<_> = options.into_iter().collect();
    ordered.sort_by_key(|(name, _)| {
        let fallback = name.ends_with(":Uncategorized");
        (!fallback, !mentioned(name), rank(name))
    });
    ordered.truncate(MAX_CHOICES);
    ordered.into_iter().collect()
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

    fn account(name: &str, declared: &[&str], posted: &[&str]) -> OpenAccount {
        OpenAccount {
            name: name.into(),
            declared: declared.iter().map(|c| c.to_string()).collect(),
            posted: posted.iter().map(|c| c.to_string()).collect(),
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
        // Hyphenated dates with matching separators count, in both readings.
        let found: Vec<_> = dates(
            "Transaction date: 11-23-2026, ref 11-23/2026",
            date("2026-11-25"),
        )
        .into_iter()
        .map(|(date, _)| date.to_string())
        .collect();
        assert_eq!(found, ["2026-11-23"]);
        // Year-first slash dates count, even when forwarded in a later month.
        assert_eq!(
            dates("Receipt date: 2026/11/23", date("2026-12-01"))[0].0,
            date("2026-11-23")
        );
        // The calendar date of an ISO timestamp counts.
        assert_eq!(
            dates("Transaction time: 2026-11-23T23:58:00+08:00", anchor)[0].0,
            date("2026-11-23")
        );
        // A yearless date after the anchor belongs to the previous year.
        assert_eq!(dates("on 30 Dec", anchor)[0].0, date("2025-12-30"));
    }

    #[test]
    fn rejects_partial_and_non_ascii_numbers() {
        assert!(amounts("€12,34 and 1.234,56 EUR and S$12.505").is_empty());
        assert!(amounts("EUR 1 234.56 and 1\u{202f}234 USD and CHF 1'234.50").is_empty());
        assert_eq!(amounts("SGD 12 on 3 Nov").len(), 1);
        assert!(amounts("€1’234.56 and R$10 and NT$300 and €12.٣٤").is_empty());
        // A code between two numbers offers both readings.
        let found: Vec<_> = amounts("Ref 123 SGD 12.50")
            .into_iter()
            .map(|(money, _)| money.number)
            .collect();
        assert_eq!(found, ["123", "12.50"]);
        assert_eq!(
            join_split_amounts("Amount\nSGD\n\n12.50\n7.25\nUSD"),
            "Amount\nSGD 12.50\n7.25 USD"
        );
        assert_eq!(join_split_amounts("$\n12.50\nCAD"), "$ 12.50 CAD");
        let found = amounts(&join_split_amounts("$\n12.50\nCAD"));
        assert_eq!(found[0].0.currency.as_deref(), Some("CAD"));
        // An itemised receipt keeps its labelled total past the candidate cap.
        let receipt: String = (1..=14)
            .map(|i| format!("Item {i} USD {i}.00\n"))
            .collect::<String>()
            + "Total paid: USD 143.27";
        let found = amounts(&receipt);
        assert_eq!(found.len(), MAX_CANDIDATES);
        assert!(found.iter().any(|(money, _)| money.number == "143.27"));
        // A payment label on the line above its amount counts.
        let receipt: String = (1..=14)
            .map(|i| format!("Item {i}\n${i}.00\n"))
            .collect::<String>()
            + "Total paid\n\n$143.27";
        let found = amounts(&receipt);
        assert!(found.iter().any(|(money, _)| money.number == "143.27"));
        // Line totals rank below the grand total.
        let receipt: String = (1..=14)
            .map(|i| format!("Item {i} Line total USD {i}.00\n"))
            .collect::<String>()
            + "Total paid: USD 143.27";
        let found = amounts(&receipt);
        assert!(found.iter().any(|(money, _)| money.number == "143.27"));
        // Codes pair with amounts on the same line only.
        let found = amounts("Card currency: CAD\n$25.00 USD and CAD $26 USD");
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].0.number.as_str(), found[0].0.currency.as_deref()),
            ("25.00", Some("USD"))
        );
        let found = amounts("Paid SGD 12.50 23 Nov 2026, then 1 234.56 EUR");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.number, "12.50");
        let currencies: Vec<_> = amounts("CA$25.00, $26.00 CAD, ¥300 RMB, AU$4, CAD $27, HKD$28")
            .into_iter()
            .map(|(money, _)| money.currency.unwrap())
            .collect();
        assert_eq!(currencies, ["CAD", "CAD", "CNY", "AUD", "CAD", "HKD"]);
        // Arabic-Indic digits are not dates and must not panic.
        assert!(dates("٢٣/١١/٢٠٢٦ ٢٠٢٦-١١-٢٣ ٢٣ Nov", date("2026-11-24")).is_empty());
    }

    #[test]
    fn bounds_header_text() {
        assert_eq!(strip_forward_prefix("FWD: fw:Re:  Alert"), "Alert");
        let subject = "fw:".repeat(1_000_000);
        assert_eq!(strip_forward_prefix(&subject), "");
        let cjk = "交".repeat(180);
        assert_eq!(truncate_bytes(&cjk, 512).len(), 510);
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

        // Table cells stay on separate lines, so "SGD 12.50" keeps its amount.
        let html = "Message-ID: <h1@bank.example>\r\nSubject: Alert\r\nContent-Type: text/html\r\n\r\n\
<table><tr><td>Amount</td><td>SGD 12.50</td><td>23 Nov 2026</td></tr>\
<tr><td>Charged</td><td>SGD</td><td>7.25</td></tr><tr><td>Total</td><td>S$9.90</td></tr>\
<tr><td><div>Paid</div></td><td><div>SGD</div></td><td><div>4.10</div></td></tr></table>\r\n";
        let email = parse(html.as_bytes()).unwrap();
        let found: Vec<_> = amounts(&email.body)
            .into_iter()
            .map(|(money, _)| money.number)
            .collect();
        assert_eq!(found, ["12.50", "7.25", "9.90", "4.10"], "{:?}", email.body);
        assert_eq!(
            email.body.lines().filter(|line| !line.is_empty()).count(),
            5,
            "{:?}",
            email.body
        );
    }

    #[tokio::test]
    async fn builds_a_signed_row_from_model_choices() {
        let email = parse(ALERT.as_bytes()).unwrap();
        let accounts = vec![
            account("Assets:DBS-Savings", &["SGD"], &[]),
            account("Liabilities:DBS-Card-1234", &[], &[]),
            account("Expenses:Coffee", &[], &[]),
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
        let statement = extract(&email, &accounts, &[], "USD", &model)
            .await
            .unwrap();
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
        let row = extract(&email, &accounts, &[], "USD", &fixed(unsure))
            .await
            .unwrap()
            .rows
            .remove(0);
        assert_eq!(
            (row.amount.as_str(), row.account.as_str()),
            ("12.50", "Income:Uncategorized")
        );

        // Closed accounts are never offered or used as a fallback.
        let closed = [
            "Expenses:Dining".to_string(),
            "Expenses:Uncategorized".to_string(),
        ];
        let mut unsure_out = answers.clone();
        unsure_out.insert(
            "category".into(),
            choice("Expenses:Coffee", &[("Expenses:Coffee", 0.1)]),
        );
        let model = fixed(unsure_out);
        let result = extract(&email, &accounts, &closed, "USD", &model).await;
        assert!(matches!(result, Err(Failure::Rejected(_))));
        let calls = model.calls.into_inner().unwrap();
        let options = calls[1]["category"]["criteria"].as_object().unwrap();
        assert!(
            !options.contains_key("Expenses:Dining") && options.contains_key("Expenses:Groceries")
        );

        // The identified account is kept: when no amount fits its currency,
        // the email is rejected rather than posted to some other account.
        let restricted = vec![
            account("Assets:USD-Wallet", &["USD"], &[]),
            account("Liabilities:DBS-Card-1234", &[], &[]),
        ];
        let mut prefers_usd = answers.clone();
        prefers_usd.insert(
            "account".into(),
            choice(
                "Assets:USD-Wallet",
                &[
                    ("Assets:USD-Wallet", 0.7),
                    ("Liabilities:DBS-Card-1234", 0.3),
                ],
            ),
        );
        let result = extract(&email, &restricted, &[], "USD", &fixed(prefers_usd)).await;
        assert!(matches!(result, Err(Failure::Rejected(_))));

        let mut promo = answers;
        promo.insert("transaction".into(), Answer::Noul { noul: 0.1 });
        assert!(matches!(
            extract(&email, &accounts, &[], "USD", &fixed(promo)).await,
            Err(Failure::Rejected(_))
        ));
    }

    #[tokio::test]
    async fn foreign_purchase_posts_the_settled_amount_to_the_named_card() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        email.body =
            "Your card ending 1234 was charged EUR 50.00 (SGD 75.30) at CAFE DE FLORE on 23 Nov 2026."
                .into();
        let accounts = vec![
            account("Liabilities:DBS-Card-1234", &["SGD"], &[]),
            account("Assets:Euro-Wallet", &["EUR"], &[]),
            account("Expenses:Dining", &["USD"], &[]),
        ];
        let answers = Answers::from([
            ("transaction".into(), Answer::Noul { noul: 0.95 }),
            ("direction".into(), choice("out", &[])),
            // The model picks the purchase price, which the SGD card cannot hold.
            ("amount".into(), choice("1", &[("1", 0.6), ("2", 0.4)])),
            ("date".into(), choice("1", &[])),
            ("payee".into(), choice("1", &[])),
            (
                "account".into(),
                choice(
                    "Liabilities:DBS-Card-1234",
                    &[
                        ("Liabilities:DBS-Card-1234", 0.8),
                        ("Assets:Euro-Wallet", 0.2),
                    ],
                ),
            ),
            (
                "category".into(),
                choice("Expenses:Groceries", &[("Expenses:Groceries", 0.6)]),
            ),
            ("settled".into(), choice("1", &[])),
        ]);
        // Without a settled amount, a balance in the card's currency is not used instead.
        let mut balance_only = answers.clone();
        balance_only.insert("settled".into(), choice("none", &[]));
        let mut balance_email = parse(ALERT.as_bytes()).unwrap();
        balance_email.body =
            "Charged EUR 50.00 to card ending 1234 on 23 Nov 2026. Available balance: SGD 1,204.33."
                .into();
        let model = fixed(balance_only);
        let result = extract(&balance_email, &accounts, &[], "USD", &model).await;
        assert!(matches!(result, Err(Failure::Rejected(_))));
        let calls = model.calls.into_inner().unwrap();
        assert!(calls[1]["settled"]["criteria"].get("none").is_some());

        let model = fixed(answers);
        let statement = extract(&email, &accounts, &[], "USD", &model)
            .await
            .unwrap();
        assert_eq!(
            (
                statement.source.as_str(),
                statement.currency.as_str(),
                statement.rows[0].amount.as_str()
            ),
            ("Liabilities:DBS-Card-1234", "SGD", "-75.30")
        );
        // Categories restricted to another currency are not offered.
        let calls = model.calls.into_inner().unwrap();
        assert_eq!(calls.len(), 3, "transaction, settled amount, then category");
        assert!(
            calls[2]["category"]["criteria"]
                .get("Expenses:Dining")
                .is_none()
        );
    }

    #[tokio::test]
    async fn bare_dollar_is_checked_in_its_resolved_currency() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        email.body =
            "Card ending 1234 charged $25.00, settled as CAD 34.50, on 23 Nov 2026.".into();
        let accounts = vec![account("Liabilities:Card-1234", &["CAD", "EUR"], &[])];
        let answers = Answers::from([
            ("transaction".into(), Answer::Noul { noul: 0.95 }),
            ("direction".into(), choice("out", &[])),
            ("amount".into(), choice("1", &[])),
            ("date".into(), choice("1", &[])),
            ("payee".into(), choice("none", &[])),
            ("settled".into(), choice("1", &[])),
            ("account".into(), choice("Liabilities:Card-1234", &[])),
            (
                "category".into(),
                choice("Expenses:Shopping", &[("Expenses:Shopping", 0.8)]),
            ),
        ]);
        // "$25.00" resolves to the default USD, which the card cannot hold.
        let statement = extract(&email, &accounts, &[], "USD", &fixed(answers))
            .await
            .unwrap();
        assert_eq!(
            (
                statement.currency.as_str(),
                statement.rows[0].amount.as_str()
            ),
            ("CAD", "-34.50")
        );
    }

    #[test]
    fn limit_keeps_the_account_matching_a_cited_card_number() {
        let options: BTreeMap<String, String> = (0..80)
            .map(|i| (format!("Assets:Bank{i:02}"), String::new()))
            .chain([("Liabilities:Visa-91234".into(), String::new())])
            .collect();
        let kept = limit(options, "Paid with card ending 1234", |_| 1);
        assert!(kept.contains_key("Liabilities:Visa-91234"));
    }

    #[test]
    fn limits_options_and_keeps_fallbacks_and_mentions() {
        let options: BTreeMap<String, String> = (0..80)
            .map(|i| (format!("Expenses:Shop{i:02}"), String::new()))
            .chain([
                ("Expenses:Uncategorized".into(), String::new()),
                ("Income:Salary".into(), String::new()),
                ("Expenses:Zeta-Bakery".into(), String::new()),
            ])
            .collect();
        let kept = limit(options, "Paid at BAKERY", |name| {
            if name.starts_with("Income:") { 4 } else { 1 }
        });
        assert_eq!(kept.len(), MAX_CHOICES);
        assert!(
            kept.contains_key("Expenses:Uncategorized")
                && kept.contains_key("Expenses:Zeta-Bakery")
        );
        assert!(!kept.contains_key("Income:Salary"));
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
        let only = vec![account("Assets:Wallet", &[], &["SGD"])];
        let mut wallet = answers.clone();
        wallet.insert("account".into(), choice("Assets:Wallet", &[]));
        let model = fixed(wallet);
        let statement = extract(&email, &only, &[], "USD", &model).await.unwrap();
        // A ledger with only a bank account is still offered a credit card.
        let calls = model.calls.into_inner().unwrap();
        let offered = calls[0]["account"]["criteria"].as_object().unwrap();
        assert!(
            ["Assets:Wallet", "Liabilities:CreditCard"]
                .iter()
                .all(|k| offered.contains_key(*k))
        );
        assert!(!offered.contains_key("none"), "no card number is cited");
        assert_eq!(
            (statement.source.as_str(), statement.currency.as_str()),
            ("Assets:Wallet", "SGD")
        );
        assert_eq!(statement.rows[0].description, "Grab");

        let mut answers = answers;
        // "none" is offered only for a cited number no numbered account carries.
        answers.insert("account".into(), choice("none", &[]));
        let numbered = vec![
            account("Liabilities:Citi-1234", &[], &[]),
            account("Assets:Wallet", &[], &[]),
        ];
        for (body, offered) in [
            (
                "Card ending 5678: you paid $8.00 to Grab on 2026-11-23.",
                true,
            ),
            (
                "Visa ending in 1234: you paid $8.00 to Grab on 2026-11-23.",
                false,
            ),
        ] {
            email.body = body.into();
            let model = fixed(answers.clone());
            let result = extract(&email, &numbered, &[], "USD", &model).await;
            let calls = model.calls.into_inner().unwrap();
            let none = calls[0]["account"]["criteria"].get("none").is_some();
            assert_eq!(none, offered, "{body}");
            if offered {
                assert!(matches!(result, Err(Failure::Rejected(_))));
            }
        }
        // "none" is asked even with one open source, and a cited number must
        // end a numeric account part, not appear inside it.
        let cases = [
            (
                vec![account("Assets:Checking-1234", &[], &[])],
                vec!["Liabilities:CreditCard".to_string()],
                "Card ending 5678: you paid $8.00 to Grab on 2026-11-23.",
            ),
            (
                vec![
                    account("Liabilities:Visa-91234567", &[], &[]),
                    account("Assets:Wallet", &[], &[]),
                ],
                vec![],
                "Visa ending 1234: you paid $8.00 to Grab on 2026-11-23.",
            ),
        ];
        for (accounts, closed, body) in cases {
            email.body = body.into();
            let model = fixed(answers.clone());
            let result = extract(&email, &accounts, &closed, "USD", &model).await;
            assert!(matches!(result, Err(Failure::Rejected(_))), "{body}");
            let calls = model.calls.into_inner().unwrap();
            assert!(
                calls[0]["account"]["criteria"].get("none").is_some(),
                "{body}"
            );
        }
        email.body = "You paid $8.00 to Grab on 2026-11-23.".into();

        // An empty ledger has no "none": the defaults are all it can use.
        answers.insert("account".into(), choice("Assets:Checking", &[]));
        let model = fixed(answers);
        let statement = extract(&email, &[], &[], "AUD", &model).await.unwrap();
        assert_eq!(
            (statement.source.as_str(), statement.currency.as_str()),
            ("Assets:Checking", "AUD")
        );
        let calls = model.calls.into_inner().unwrap();
        assert!(calls[0]["account"]["criteria"].get("none").is_none());
    }
}
