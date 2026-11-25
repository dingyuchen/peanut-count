//! Turns a forwarded transaction email into a one-row statement.
//!
//! Clairic reads the transaction's fields from the email's text. Each field is
//! then checked against that text: the amount must be a number written in the
//! email, a currency must be written attached to it, and a date, payee, or
//! card number must appear in it. A second call then has Clairic choose the
//! user's account and the category among the ledger's accounts.

use std::{collections::BTreeMap, sync::LazyLock};

use anyhow::{Context, Result};
use chrono::{Datelike, Days, NaiveDate};
use mail_parser::{MessageParser, MessagePart, MessageRef, PartKind, html_to_text};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{
    clairic::{Answers, Extract, Field, Kind, Request},
    ledger::OpenAccount,
    mobile::{Statement, StatementRow},
};

/// Longest email text, in characters, that is sent to the model.
const MAX_TEXT: usize = 12_000;
/// Longest subject or sender, in characters.
const MAX_HEADER: usize = 300;
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
    /// The HTML alternative's text, when the body came from plain text.
    pub alternative: Option<String>,
}

#[derive(Debug)]
pub enum Failure {
    /// The email cannot become a transaction. Retrying will not help.
    Rejected(String),
    /// A model failed. Retrying may help.
    Unavailable(anyhow::Error),
}

fn rejected(reason: impl Into<String>) -> Failure {
    Failure::Rejected(reason.into())
}

/// Parses a raw message. mail-parser 1.x bounds nesting depth, the number
/// of parts, and the levels of transfer-encoded nested messages it decodes,
/// finds boundaries in linear time, and decodes bodies only when read.
pub fn parse(raw: &[u8]) -> Result<Email> {
    let message = MessageParser::default()
        .parse(raw)
        .context("input is not an email message")?;
    Ok(read(message.root(), raw))
}

fn read(outer: MessageRef, raw: &[u8]) -> Email {
    // A forward "as attachment" carries the original alert as message/rfc822.
    let message = outer
        .attachments()
        .find_map(|part| part.nested())
        .unwrap_or(outer);
    let id = match message.message_id().or(outer.message_id()) {
        Some(message_id) => fingerprint(&["peanut-email", message_id.trim()]),
        None => format!("{:x}", Sha256::digest(raw)),
    };
    let from = message
        .from()
        .and_then(|address| address.first())
        .map(|mailbox| {
            let address = mailbox.address().unwrap_or_default();
            match mailbox.name() {
                Some(name) => format!("{name} <{address}>"),
                None => address.to_owned(),
            }
        })
        .unwrap_or_default();
    let sent = message.date().and_then(|date| {
        NaiveDate::from_ymd_opt(date.year.into(), date.month.into(), date.day.into())
    });
    let bounded = |text: String| {
        truncate_lines(&clean(truncate_lines(&text, MAX_TEXT * 4)), MAX_TEXT).to_owned()
    };
    let (body, alternative) = body_text(message);
    Email {
        id,
        from: clean(truncate(&from, MAX_HEADER)),
        subject: strip_forward_prefix(&clean(truncate(
            message.subject().unwrap_or_default(),
            MAX_HEADER,
        )))
        .to_owned(),
        sent,
        // Bound the text before cleaning it, leaving room for the whitespace
        // that cleaning removes, and cut at a line so no value is split.
        body: bounded(body),
        alternative: alternative.map(bounded),
    }
}

static BLOCK_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<(/?(?:tr|p|div|br|li|h[1-6]|table|section|header|footer)\b)").unwrap()
});

static CELL_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<(/?t[dh]\b)").unwrap());

/// The first text body, and the HTML alternative's text when the body is
/// plain text. HTML gets a line break at each row and block tag and a space
/// between table cells, so a row such as "Total | SGD 12.50" reads as one line.
fn body_text(message: MessageRef) -> (String, Option<String>) {
    let is_html = |part: &MessagePart| matches!(part.kind(), PartKind::Html);
    let as_text = |part: MessagePart| {
        let body = part.text().unwrap_or_default();
        if !is_html(&part) {
            return body.into_owned();
        }
        // html_to_text collapses whitespace, so mark the breaks with
        // private-use characters and restore them afterwards.
        let html = BLOCK_TAG.replace_all(&body, "\u{e000}<$1");
        let html = CELL_TAG.replace_all(&html, "\u{e001}<$1");
        html_to_text(&html)
            .replace('\u{e000}', "\n")
            .replace('\u{e001}', " ")
    };
    let html_part = message.html_body().find(is_html);
    match message.text_body().find(MessagePart::is_text) {
        Some(part) if is_html(&part) => (as_text(part), None),
        Some(part) => (as_text(part), html_part.map(as_text)),
        None => (html_part.map(as_text).unwrap_or_default(), None),
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

/// Like [`truncate`], but ends at the last line break (or, for one long line,
/// whitespace) within the limit, so no amount or date is cut in half: a
/// "USD 125.00" cut to "USD 12" would read as a smaller amount.
fn truncate_lines(text: &str, chars: usize) -> &str {
    let cut = truncate(text, chars);
    if cut.len() == text.len() {
        return text;
    }
    match cut.rfind('\n').or_else(|| cut.rfind(char::is_whitespace)) {
        Some(end) => &cut[..end],
        None => "",
    }
}

fn truncate(text: &str, chars: usize) -> &str {
    text.char_indices()
        .nth(chars)
        .map_or(text, |(at, _)| &text[..at])
}

/// A number as written in the text: optionally grouped with commas or
/// apostrophes, with an optional decimal fraction, or a bare fraction such as
/// ".50".
static WRITTEN_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9]{1,3}(?:[,'’][0-9]{3})+(?:\.[0-9]+)?|[0-9]+(?:\.[0-9]+)?|\.[0-9]+").unwrap()
});

/// A number without grouping, leading zeros, or trailing fraction zeros, so
/// "1,234.50" and "1234.5" compare equal.
fn canonical(number: &str) -> Option<String> {
    let plain: String = number
        .chars()
        .filter(|c| !matches!(c, ',' | '\'' | '’' | ' '))
        .collect();
    let (whole, fraction) = plain.split_once('.').unwrap_or((&plain, ""));
    if (whole.is_empty() && fraction.is_empty())
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole = match whole.trim_start_matches('0') {
        "" => "0",
        whole => whole,
    };
    let fraction = fraction.trim_end_matches('0');
    Some(if fraction.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{fraction}")
    })
}

/// The amount as written in `text`, without grouping, when the model's value
/// is a nonzero number that the text states. A value the model computed or
/// invented is not in the text and returns `None`.
#[cfg(test)]
fn written_amount(text: &str, value: &str) -> Option<String> {
    written_amounts(text, value)
        .into_iter()
        .next()
        .map(|(amount, _)| amount)
}

/// Every place `text` writes the model's amount, as written without grouping,
/// with the byte range of the number.
fn written_amounts(text: &str, value: &str) -> Vec<(String, std::ops::Range<usize>)> {
    let Some(wanted) = canonical(value.trim().trim_start_matches(['-', '+', '−'])) else {
        return Vec::new();
    };
    if wanted.len() > 24 || !wanted.bytes().any(|b| matches!(b, b'1'..=b'9')) {
        return Vec::new();
    }
    let digit = |c: Option<char>| c.is_some_and(|c| c.is_ascii_digit());
    let separator = |c: Option<char>| matches!(c, Some('.' | ',' | '\'' | '’'));
    WRITTEN_NUMBER
        .find_iter(text)
        .filter_map(|found| {
            // Skip part of a longer number, such as the "12" of "12.505" or the
            // "50" of "1.50".
            let mut after = text[found.end()..].chars();
            let mut before = text[..found.start()].chars().rev();
            let (next, prev) = (after.next(), before.next());
            let fragment = digit(next)
                || digit(prev)
                || (separator(next) && digit(after.next()))
                || (separator(prev) && digit(before.next()));
            let as_written = found.as_str().replace([',', '\'', '’'], "");
            let as_written = if as_written.starts_with('.') {
                format!("0{as_written}")
            } else {
                as_written
            };
            (!fragment && canonical(&as_written).as_deref() == Some(wanted.as_str()))
                .then(|| (as_written, found.range()))
        })
        .collect()
}

/// Symbols other than "$" that name exactly one currency.
const SYMBOLS: [(&str, &str); 8] = [
    ("€", "EUR"),
    ("£", "GBP"),
    ("₹", "INR"),
    ("₩", "KRW"),
    ("₱", "PHP"),
    ("฿", "THB"),
    ("₫", "VND"),
    ("₪", "ILS"),
];

/// Letters written directly before "$" that name one dollar currency.
const DOLLARS: [(&str, &str); 12] = [
    ("US", "USD"),
    ("S", "SGD"),
    ("SG", "SGD"),
    ("A", "AUD"),
    ("AU", "AUD"),
    ("C", "CAD"),
    ("CA", "CAD"),
    ("HK", "HKD"),
    ("NZ", "NZD"),
    ("NT", "TWD"),
    ("R", "BRL"),
    ("MX", "MXN"),
];

/// Whether the currency `code` is written attached to the amount at `span`:
/// directly before it, ignoring spaces, line breaks, and a sign, as in
/// "USD 12.50", "S$12.50", "CAD $12.50", "Amount (CAD): 25.00", or "USD" on
/// the line above;
/// or directly after it, as in "12.50 USD", "12.50 (USD)", or "128.50元". A
/// code directly followed by another number belongs to that number. Only
/// Latin letters touching a code make it part of a word, so "消费USD12.50"
/// states USD. A bare "$" or "¥", which several currencies share, states
/// none, unless 元, RMB, or 円 elsewhere settles "¥".
fn attached_currency(text: &str, span: &std::ops::Range<usize>, code: &str) -> bool {
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_uppercase()) {
        return false;
    }
    let letter = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphabetic());
    let before = text[..span.start]
        .trim_end()
        .trim_end_matches(['-', '−', '+'])
        .trim_end();
    let after = text[span.end..].trim_start();
    // "CODE" ending `before`, as a word.
    let code_before = |before: &str| {
        before.ends_with(code) && !letter(before[..before.len() - 3].chars().next_back())
    };
    let labelled = before
        .trim_end_matches(':')
        .trim_end()
        .strip_suffix(')')
        .and_then(|rest| rest.strip_suffix(code))
        .is_some_and(|rest| rest.ends_with('('));
    let dollar = before.strip_suffix('$').is_some_and(|rest| {
        let prefix_len = rest.len()
            - rest
                .trim_end_matches(|c: char| c.is_ascii_alphabetic())
                .len();
        let prefix = &rest[rest.len() - prefix_len..];
        DOLLARS
            .iter()
            .any(|(known, named)| *named == code && prefix.eq_ignore_ascii_case(known))
    });
    let symbol_before = SYMBOLS
        .iter()
        .any(|(symbol, named)| *named == code && before.ends_with(symbol));
    // A code, then a symbol, as in "CAD $12.50" or "EUR €12.50".
    let code_then_symbol = SYMBOLS
        .iter()
        .map(|(symbol, _)| *symbol)
        .chain(["$", "¥"])
        .filter_map(|symbol| before.strip_suffix(symbol))
        .any(|rest| code_before(rest.trim_end()));
    let yen = (before.ends_with('¥') || after.starts_with('¥'))
        && match code {
            "CNY" => text.contains('元') || text.contains("RMB"),
            "JPY" => text.contains('円'),
            _ => false,
        };
    let code_after = after
        .strip_prefix('(')
        .and_then(|rest| rest.strip_prefix(code))
        .is_some_and(|rest| rest.starts_with(')'))
        || after.strip_prefix(code).is_some_and(|rest| {
            // Not "SGD 75.30", the next amount's own code.
            !letter(rest.chars().next())
                && !rest.trim_start().starts_with(|c: char| c.is_ascii_digit())
        });
    let symbol_after = SYMBOLS
        .iter()
        .any(|(symbol, named)| *named == code && after.starts_with(symbol))
        || (code == "CNY" && after.starts_with('元'))
        || (code == "JPY" && after.starts_with('円'));
    code_before(before)
        || code_then_symbol
        || labelled
        || dollar
        || symbol_before
        || yen
        || code_after
        || symbol_after
}

/// The currencies [`attached_currency`] can recognize by code or symbol.
fn known_currencies() -> impl Iterator<Item = &'static str> {
    let mut codes: Vec<&str> = SYMBOLS
        .iter()
        .chain(DOLLARS.iter())
        .map(|(_, code)| *code)
        .chain(["CNY", "JPY"])
        .collect();
    codes.sort_unstable();
    codes.dedup();
    codes.into_iter()
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// The model's ISO date when it is plausible and one date expression in the
/// text writes its day and month together, as digits or with a month name.
/// A year written in that expression must match; a yearless expression, as
/// many alerts write, takes the model's inferred year.
fn written_date(text: &str, value: &str, latest: NaiveDate) -> Option<NaiveDate> {
    let date = NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").ok()?;
    let first = NaiveDate::from_ymd_opt(1900, 1, 1)?;
    if !(first..=latest).contains(&date) {
        return None;
    }
    let (d, m, mon) = (date.day(), date.month(), MONTHS[date.month0() as usize]);
    let year = r"(?P<year>[0-9]{4}|[0-9]{2})";
    let patterns = [
        format!(r"(?i)\b(?P<year>[0-9]{{4}})[-/.]0?{m}[-/.]0?{d}(?:T|\b)"),
        // A dotted date needs its year, so the "12.05" of an amount is not 12 May.
        format!(r"\b0?{d}[/-]0?{m}(?:[/.-]{year})?\b"),
        format!(r"\b0?{m}[/-]0?{d}(?:[/.-]{year})?\b"),
        format!(r"\b0?{d}\.0?{m}\.{year}\b"),
        format!(r"\b0?{m}\.0?{d}\.{year}\b"),
        format!(r"(?i)\b0?{d}(?:st|nd|rd|th)?[ -]*{mon}[a-z]*\.?(?:[ ,-]+{year})?\b"),
        format!(r"(?i)\b{mon}[a-z]*\.?[ -]*0?{d}(?:st|nd|rd|th)?\b(?:,? +{year}\b)?"),
    ];
    // An expression that starts right after a digit and a separator is the
    // tail of a longer date, as the "11-23" of "2026-11-23".
    let tail = |caps: &regex::Captures| {
        let mut before = text[..caps.get(0).unwrap().start()].chars().rev();
        matches!(before.next(), Some('/' | '.' | '-'))
            && before.next().is_some_and(|c| c.is_ascii_digit())
    };
    let agrees = |caps: &regex::Captures| match caps.name("year") {
        None => true,
        Some(written) => {
            let value: i32 = written.as_str().parse().unwrap_or(-1);
            let value = if written.len() == 2 {
                2000 + value
            } else {
                value
            };
            // A number that runs on is an hour or an amount, not a year, as in
            // "23 Nov 10:15" or "23 Nov 25.00".
            let rest = &text[written.end()..];
            let runs_on = rest.starts_with(':')
                || (rest.starts_with(['.', ','])
                    && rest[1..].starts_with(|c: char| c.is_ascii_digit()));
            value == date.year() || runs_on
        }
    };
    patterns
        .iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .any(|pattern| {
            pattern
                .captures_iter(text)
                .any(|caps| !tail(&caps) && agrees(&caps))
        })
        .then_some(date)
}

/// The text with whitespace collapsed and lower-cased, for finding a phrase.
fn plain(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// `value` when the text contains it, ignoring case and spacing.
fn written_phrase(text: &str, value: &str) -> Option<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (value.chars().any(char::is_alphanumeric) && plain(text).contains(&value.to_lowercase()))
        .then(|| truncate(&value, 200).to_owned())
}

/// The digits of a card or account number when the text writes them at the
/// end of a number, ignoring spaces and hyphens between digit groups, so
/// "123-456 789" contains "456789" but "12345678" does not contain "3456".
fn written_ending(text: &str, value: &str) -> Option<String> {
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    if !(3..=19).contains(&digits.len()) {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    let compact: String = chars
        .iter()
        .enumerate()
        .filter(|&(i, c)| {
            let between = i > 0
                && chars[i - 1].is_ascii_digit()
                && chars.get(i + 1).is_some_and(char::is_ascii_digit);
            !(matches!(c, ' ' | '-') && between)
        })
        .map(|(_, c)| *c)
        .collect();
    compact
        .match_indices(&digits)
        .any(|(at, _)| !compact[at + digits.len()..].starts_with(|c: char| c.is_ascii_digit()))
        .then_some(digits)
}

/// Whether an account name carries a number that matches a cited number,
/// where either may be the other's ending. Consecutive numeric parts of the
/// name form one number, so "Checking-123-45678-9012" is 123456789012: it
/// matches "ending 9012" and "123-45678-9012", but not "987-65432-9012".
/// "Citi-1234" matches "ending 1234" and "4111 1111 1111 1234", and
/// "Visa-91234567" does not match "1234".
fn names_number(name: &str, number: &str) -> bool {
    let mut numbers = Vec::new();
    let mut run = String::new();
    for part in name.split([':', '-']) {
        if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) {
            run.push_str(part);
        } else if !run.is_empty() {
            numbers.push(std::mem::take(&mut run));
        }
    }
    numbers.push(run);
    numbers
        .iter()
        .any(|own| own.len() >= 3 && (own.ends_with(number) || number.ends_with(own.as_str())))
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

/// What the extraction model reports. Every field is checked against the
/// email's text before it is used.
#[derive(Debug, Default, Deserialize)]
pub struct Extraction {
    pub is_transaction: Option<bool>,
    pub direction: Option<String>,
    pub amount: Option<String>,
    pub currency: Option<String>,
    pub settled_amount: Option<String>,
    pub settled_currency: Option<String>,
    pub date: Option<String>,
    pub payee: Option<String>,
    pub account_ending: Option<String>,
}

const FIELDS: &[Field<'static>] = &[
    Field {
        name: "is_transaction",
        kind: Kind::Boolean,
        instructions: "True only if the email reports one completed or authorised payment, purchase, transfer, withdrawal, refund, or deposit with a stated amount. Promotions, statements, balance summaries, bills not yet paid, one-time passwords, and login alerts are false.",
    },
    Field {
        name: "direction",
        kind: Kind::Choice(&["out", "in"]),
        instructions: "out if money left the user's account or was charged to it; in if money arrived or was credited (refunds, deposits, salary, transfers in).",
    },
    Field {
        name: "amount",
        kind: Kind::Number,
        instructions: "The full amount of money that moved. For a receipt, the total paid, not a line item. Never an account balance or credit limit.",
    },
    Field {
        name: "currency",
        kind: Kind::Code,
        instructions: "The three-letter ISO 4217 code of that amount, such as SGD for S$ or EUR for €. Null if the email writes only a bare $ or no currency.",
    },
    Field {
        name: "settled_amount",
        kind: Kind::Number,
        instructions: "Only for a foreign-currency transaction that also states a second, converted amount in the account's own currency, such as the 75.30 in 'EUR 50.00 (SGD 75.30)': that second amount. Null when the email states only one amount.",
    },
    Field {
        name: "settled_currency",
        kind: Kind::Code,
        instructions: "The currency of the second, converted amount: the code written next to it, such as SGD in 'EUR 50.00 (SGD 75.30)'. Never the currency of the first amount. Null when the email states only one amount.",
    },
    Field {
        name: "date",
        kind: Kind::Date,
        instructions: "The date of the transaction.",
    },
    Field {
        name: "payee",
        kind: Kind::Text,
        instructions: "The merchant, person, or organisation on the other side, exactly as written.",
    },
    Field {
        name: "account_ending",
        kind: Kind::Text,
        instructions: "The digits of the user's card or account number that the email names, such as 1234 for 'card ending 1234'.",
    },
];

/// The model's answer as an [`Extraction`], or `None` when a field is missing
/// or has a type its kind does not allow, so a malformed answer is retried
/// rather than read as a missing value. Numbers become text, as the email
/// would write them without grouping.
fn read_answer(answers: Answers) -> Option<Extraction> {
    let mut fields: Map<String, Value> = answers
        .into_iter()
        .map(|(name, answer)| (name, answer.value))
        .collect();
    for field in FIELDS {
        let value = fields.get_mut(field.name)?;
        let allowed = match (field.kind, &*value) {
            (_, Value::Null) => true,
            (Kind::Boolean, value) => value.is_boolean(),
            (Kind::Choice(words), Value::String(word)) => words.contains(&word.as_str()),
            (Kind::Number, Value::Number(number)) => {
                *value = Value::String(number.to_string());
                true
            }
            (Kind::Number | Kind::Text | Kind::Code | Kind::Date, value) => value.is_string(),
            _ => false,
        };
        if !allowed {
            return None;
        }
    }
    serde_json::from_value(Value::Object(fields)).ok()
}

/// Words that report money which may not have moved.
static DECLINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:declined?|failed|failure|unsuccessful|cancell?ed|cancell?ation|rejected|reversed|voided|not (?:been )?(?:approved|processed|completed))\b",
    )
    .unwrap()
});

/// The fields of a transaction read from one body of an email, each checked
/// against that body's text.
struct Reading {
    text: String,
    message: String,
    outgoing: bool,
    amount: String,
    currency: Option<String>,
    /// Currencies written attached to the amount, whatever the model said.
    written: Vec<&'static str>,
    settled: Option<(String, String)>,
    date: NaiveDate,
    payee: Option<String>,
    ending: Option<String>,
}

/// Reads the transaction from `body`, rejecting it when the model says it is
/// none or its amount is not written in the text.
async fn read_body(email: &Email, body: &str, reader: &impl Extract) -> Result<Reading, Failure> {
    let text = format!("{}\n{}", email.subject, body);
    if text.trim().is_empty() {
        return Err(rejected("The email has no readable text."));
    }
    let message = format!(
        "From: {}\nSubject: {}\nSent: {}\n\n{}",
        email.from,
        email.subject,
        email.sent.map(|date| date.to_string()).unwrap_or_default(),
        body,
    );
    let raw = reader
        .extract(&Request {
            text: &message,
            reference_date: email.sent,
            fields: FIELDS,
        })
        .await
        .map_err(Failure::Unavailable)?;
    let found = read_answer(raw).ok_or_else(|| {
        Failure::Unavailable(anyhow::anyhow!(
            "extraction model answer does not match the schema"
        ))
    })?;
    if found.is_transaction != Some(true) {
        return Err(rejected(
            "The email does not look like a single completed transaction.",
        ));
    }
    let outgoing = match found.direction.as_deref() {
        Some("out") => true,
        Some("in") => false,
        _ => {
            return Err(rejected(
                "The email does not say whether money went out or came in.",
            ));
        }
    };
    let places = found
        .amount
        .as_deref()
        .map(|value| written_amounts(&text, value))
        .unwrap_or_default();
    let Some((amount, _)) = places.first().cloned() else {
        return Err(rejected(
            "The transaction amount is not written in the email.",
        ));
    };
    // A currency counts only where it is written attached to that amount.
    let attached = |places: &[(String, std::ops::Range<usize>)], code: &Option<String>| {
        code.as_deref()
            .map(|code| code.trim().to_ascii_uppercase())
            .filter(|code| {
                places
                    .iter()
                    .any(|(_, span)| attached_currency(&text, span, code))
            })
    };
    let currency = attached(&places, &found.currency);
    let written = known_currencies()
        .filter(|code| {
            places
                .iter()
                .any(|(_, span)| attached_currency(&text, span, code))
        })
        .collect();
    // A settled amount without its own written currency could be anything,
    // so it is used only with one.
    let settled = found.settled_amount.as_deref().and_then(|value| {
        let places = written_amounts(&text, value);
        let (amount, _) = places.first().cloned()?;
        Some((amount, attached(&places, &found.settled_currency)?))
    });
    let latest = email
        .sent
        .unwrap_or_else(|| chrono::Utc::now().date_naive())
        + Days::new(1);
    let date = found
        .date
        .as_deref()
        .and_then(|value| written_date(&text, value, latest))
        .or(email.sent)
        .unwrap_or_else(|| chrono::Utc::now().date_naive());
    let payee = found
        .payee
        .as_deref()
        .and_then(|value| written_phrase(&text, value));
    let ending = found
        .account_ending
        .as_deref()
        .and_then(|value| written_ending(&text, value));
    Ok(Reading {
        text,
        message,
        outgoing,
        amount,
        currency,
        written,
        settled,
        date,
        payee,
        ending,
    })
}

/// Chooses a statement row for `email` against the ledger's open accounts.
pub async fn extract(
    email: &Email,
    accounts: &[OpenAccount],
    closed: &[String],
    default_currency: &str,
    model: &impl Extract,
) -> Result<Statement, Failure> {
    let is_closed = |name: &str| closed.iter().any(|closed| closed == name);
    // A plain-text alternative can be a stub such as "View this alert online"
    // while the HTML alternative holds the receipt, so try that before
    // rejecting the email.
    let reading = match read_body(email, &email.body, model).await {
        Err(Failure::Rejected(reason)) => match &email.alternative {
            Some(html) => read_body(email, html, model).await?,
            None => return Err(Failure::Rejected(reason)),
        },
        other => other?,
    };
    let Reading {
        text,
        message,
        outgoing,
        amount,
        currency,
        written,
        settled,
        date,
        payee,
        ending,
    } = reading;
    let mut sources: BTreeMap<String, String> = accounts
        .iter()
        .map(|account| &account.name)
        .filter(|name| name.starts_with("Assets:") || name.starts_with("Liabilities:"))
        .map(|name| (name.clone(), describe_account(name, false)))
        .collect();
    // The ledger may lack the card or account the email names. When the email
    // cites a number, such as "ending 5678", that no account name ends with,
    // the model may answer "none". Unnumbered accounts stay on offer, so
    // "Liabilities:Amex" can still match an Amex alert by name.
    let offer_none = ending
        .as_deref()
        .is_some_and(|ending| !sources.keys().any(|name| names_number(name, ending)));
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
    let sources = limit(sources, &text, ending.as_deref(), |_| 1);
    // With one account on offer, the source is known before asking.
    let known = (sources.len() == 1 && !offer_none).then(|| sources.keys().next().unwrap().clone());
    let find = |name: &str| accounts.iter().find(|account| account.name == name);
    // Accounts created on import accept any currency.
    let accepts = |name: &str, currency: &str| find(name).is_none_or(|a| a.accepts(currency));
    // The amount and currency to post to `source`.
    let post = |source: &str| -> Result<(String, String), Failure> {
        // An amount whose currency the email does not state, such as a bare
        // "$", takes the account's only currency, or the configured default.
        let resolved = currency.clone().unwrap_or_else(|| {
            let account = find(source).cloned().unwrap_or_default();
            match (&account.declared[..], &account.posted[..]) {
                ([only], _) | ([], [only]) => only.clone(),
                _ => default_currency.to_owned(),
            }
        });
        // Keep the account the email identifies. A stated settled amount is
        // what the account was actually charged or credited, so it wins when
        // the account accepts it; otherwise use the amount itself, unless the
        // email writes a different currency with it. Rejection reasons reach
        // the relay or a bounce, so they name no account and no currency.
        match &settled {
            Some((settled, code)) if accepts(source, code) => Ok((settled.clone(), code.clone())),
            _ if !written.is_empty() && !written.contains(&resolved.as_str()) => Err(rejected(
                "The currency written with the amount could not be confirmed.",
            )),
            _ if accepts(source, &resolved) => Ok((amount.clone(), resolved)),
            _ => Err(rejected(
                "The email has no settled amount in a currency the account accepts.",
            )),
        }
    };
    if let Some(source) = &known {
        let (_, currency) = post(source)?;
        counters.remove(source);
        counters.retain(|name, _| accepts(name, &currency));
    }
    let counters = limit(counters, &text, ending.as_deref(), |name| {
        let root = name.split(':').next().unwrap_or_default();
        match (root, outgoing) {
            ("Expenses", true) | ("Income", false) => 1,
            ("Expenses", false) => 2,
            ("Income", true) => 4,
            _ => 3,
        }
    });

    // Ask every question in one call. Each is a choice among account names,
    // with what each option means written into its instructions.
    let menu = |options: &BTreeMap<String, String>| {
        options
            .iter()
            .map(|(name, meaning)| format!("{name}: {meaning}."))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut source_options: Vec<&str> = sources.keys().map(String::as_str).collect();
    let mut source_menu = menu(&sources);
    if offer_none {
        source_options.push("none");
        source_menu.push_str(
            " none: none of these; the account this transaction moves money in or out of is not listed.",
        );
    }
    let source_instructions = format!(
        "Which of the user's accounts did the transaction move money {}? For a transfer between two of the user's accounts, the one the email reports as {}, if it is listed. Match bank names and card or account numbers where possible. Options: {source_menu}",
        if outgoing { "out of" } else { "into" },
        if outgoing {
            "debited or charged"
        } else {
            "credited"
        },
    );
    let category_options: Vec<&str> = counters.keys().map(String::as_str).collect();
    let category_instructions = format!(
        "{} Options: {}",
        if outgoing {
            "Money left the user's account. Which account best describes what it was spent on, or which of the user's own accounts received it?"
        } else {
            "Money arrived in the user's account. Which account best describes where it came from? For a refund, the spending category it reverses. For a transfer, the user's own account it came from."
        },
        menu(&counters)
    );
    let mut questions = vec![Field {
        name: "category",
        kind: Kind::Choice(&category_options),
        instructions: &category_instructions,
    }];
    if known.is_none() {
        questions.push(Field {
            name: "account",
            kind: Kind::Choice(&source_options),
            instructions: &source_instructions,
        });
    }
    // An email that mentions a decline or cancellation may report money that
    // never moved, or a refund for a cancelled order that did, so ask.
    if DECLINE.is_match(&text) {
        questions.push(Field {
            name: "moved",
            kind: Kind::Boolean,
            instructions: "Did money actually move in the transaction the email reports? False if the payment, purchase, or transfer was declined, failed, or cancelled before it completed. A refund, including one for a cancelled order, did move money.",
        });
    }
    let answers = model
        .extract(&Request {
            text: &message,
            reference_date: email.sent,
            fields: &questions,
        })
        .await
        .map_err(Failure::Unavailable)?;
    let chosen = |name: &str| answers.get(name).map(|answer| &answer.value);
    if DECLINE.is_match(&text) && chosen("moved") != Some(&Value::Bool(true)) {
        return Err(rejected(
            "The email reports a declined, failed, or cancelled transaction.",
        ));
    }
    let source = match known {
        Some(source) => source,
        None => match chosen("account") {
            Some(Value::String(name)) if name == "none" && offer_none => {
                return Err(rejected(
                    "The email names an account that is not in the ledger. Open it, then forward the email again.",
                ));
            }
            Some(Value::String(name)) if sources.contains_key(name) => name.clone(),
            Some(Value::Null) => {
                return Err(rejected("The model could not choose the account."));
            }
            _ => {
                return Err(Failure::Unavailable(anyhow::anyhow!(
                    "model gave no valid `account` answer"
                )));
            }
        },
    };

    let (number, currency) = post(&source)?;

    // The category was asked before the source was known, so it may name the
    // source itself, or an account that does not take the currency; then, or
    // when the model is unsure, fall back to Uncategorized.
    let category = answers
        .get("category")
        .and_then(|answer| match &answer.value {
            Value::String(name)
                if counters.contains_key(name)
                    && *name != source
                    && accepts(name, &currency)
                    && answer.confidence.unwrap_or(1.0) >= MIN_CATEGORY =>
            {
                Some(name.clone())
            }
            _ => None,
        });
    let account = match category {
        Some(account) => account,
        None => {
            let fallback = if outgoing {
                "Expenses:Uncategorized"
            } else {
                "Income:Uncategorized"
            };
            if is_closed(fallback) || !accepts(fallback, &currency) {
                return Err(rejected(
                    "No category fits, and the fallback category is closed or does not accept the transaction's currency.",
                ));
            }
            fallback.to_owned()
        }
    };

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
                format!("-{number}")
            } else {
                number
            },
            account,
        }],
    })
}

/// Most options offered in one question, to keep its instructions short.
const MAX_CHOICES: usize = 50;

/// Keeps at most [`MAX_CHOICES`] options: the Uncategorized fallbacks, then
/// accounts named by the card number the email cites, then accounts whose
/// name parts appear in `text`, then by `rank`, then by name.
fn limit(
    options: BTreeMap<String, String>,
    text: &str,
    ending: Option<&str>,
    rank: impl Fn(&str) -> u8,
) -> BTreeMap<String, String> {
    if options.len() <= MAX_CHOICES {
        return options;
    }
    let text = text.to_lowercase();
    let card = |name: &str| ending.is_some_and(|ending| names_number(name, ending));
    let mentioned = |name: &str| {
        name.split([':', '-'])
            .skip(1)
            .any(|part| part.len() >= 3 && text.contains(&part.to_lowercase()))
    };
    let mut ordered: Vec<_> = options.into_iter().collect();
    ordered.sort_by_key(|(name, _)| {
        let fallback = name.ends_with(":Uncategorized");
        (!fallback, !card(name), !mentioned(name), rank(name))
    });
    ordered.truncate(MAX_CHOICES);
    ordered.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::clairic::Answer;

    const ALERT: &str = "Message-ID: <alert-1@bank.example>\r\n\
From: Bank Alerts <alerts@bank.example>\r\n\
Subject: Fwd: Card transaction alert\r\n\
Date: Tue, 24 Nov 2026 09:15:00 +0800\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
A transaction of S$12.50 was made with your card ending 1234 on 23 Nov 2026 at STARBUCKS VIVOCITY.\r\n\
Available balance: SGD 1,204.33.\r\n";

    /// Answers decision questions from a fixed map, null where it has no
    /// answer, and records each call's questions as Clairic receives them.
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

    impl Fixed {
        fn decide(&self, request: &Request<'_>) -> Answers {
            let questions = crate::clairic::specs(request.fields);
            self.calls.lock().unwrap().push(Value::Object(questions));
            request
                .fields
                .iter()
                .map(|field| {
                    let answer = self.answers.get(field.name).cloned();
                    (
                        field.name.to_owned(),
                        answer.unwrap_or_else(|| value(Value::Null)),
                    )
                })
                .collect()
        }
    }

    /// The model: `reader` answers the extraction and `decider` the decision
    /// questions.
    struct Model<'a, R> {
        reader: &'a R,
        decider: &'a Fixed,
    }

    impl<R: Extract + Sync> Extract for Model<'_, R> {
        async fn extract(&self, request: &Request<'_>) -> Result<Answers> {
            if request
                .fields
                .iter()
                .any(|field| field.name == "is_transaction")
            {
                self.reader.extract(request).await
            } else {
                Ok(self.decider.decide(request))
            }
        }
    }

    async fn extract(
        email: &Email,
        accounts: &[OpenAccount],
        closed: &[String],
        currency: &str,
        reader: &(impl Extract + Sync),
        decider: &Fixed,
    ) -> Result<Statement, Failure> {
        super::extract(
            email,
            accounts,
            closed,
            currency,
            &Model { reader, decider },
        )
        .await
    }

    fn value(value: Value) -> Answer {
        Answer {
            value,
            confidence: None,
        }
    }

    /// Answers with the values of a JSON object.
    fn answers(object: Value) -> Answers {
        let Value::Object(fields) = object else {
            panic!("expected an object");
        };
        fields
            .into_iter()
            .map(|(name, found)| (name, value(found)))
            .collect()
    }

    /// Returns a fixed extraction, filling unset fields with null.
    struct Reads(Value);

    impl Extract for Reads {
        async fn extract(&self, _: &Request<'_>) -> Result<Answers> {
            let mut found = json!({
                "is_transaction": true, "direction": "out", "amount": null, "currency": null,
                "settled_amount": null, "settled_currency": null, "date": null, "payee": null,
                "account_ending": null,
            });
            for (key, value) in self.0.as_object().unwrap() {
                found[key] = value.clone();
            }
            Ok(answers(found))
        }
    }

    /// A choice, with the model's confidence where `probabilities` names it.
    fn choice(chosen: &str, probabilities: &[(&str, f64)]) -> Answer {
        Answer {
            value: json!(chosen),
            confidence: probabilities
                .iter()
                .find(|(name, _)| *name == chosen)
                .map(|(_, p)| *p),
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
    fn amounts_must_be_written_in_the_email() {
        let text = "Total paid: S$1,234.50. Fee SGD 12.505. Balance SGD 0.00";
        assert_eq!(written_amount(text, "1234.50").as_deref(), Some("1234.50"));
        assert_eq!(written_amount(text, "1,234.5").as_deref(), Some("1234.50"));
        assert_eq!(
            written_amount("CHF 1'234.50", "1234.50").as_deref(),
            Some("1234.50")
        );
        // Part of a longer number, a computed value, and zero are not amounts.
        assert_eq!(written_amount(text, "12.50"), None);
        assert_eq!(written_amount(text, "1246.00"), None);
        assert_eq!(written_amount(text, "0.00"), None);
        assert_eq!(written_amount(text, "about 12"), None);
        // A bare fraction is read whole, never its digits alone.
        assert_eq!(
            written_amount("Fee USD .50", ".50").as_deref(),
            Some("0.50")
        );
        assert_eq!(written_amount("Fee USD .50", "50"), None);
    }

    #[test]
    fn currencies_must_be_attached_to_the_amount() {
        let stated = |text: &str, value: &str, code: &str| {
            written_amounts(text, value)
                .iter()
                .any(|(_, span)| attached_currency(text, span, code))
        };
        assert!(stated("Paid SGD 12.50", "12.50", "SGD"));
        assert!(stated("Paid S$12.50", "12.50", "SGD"));
        assert!(stated("Paid €12.50", "12.50", "EUR"));
        assert!(stated("Paid SGD12.50", "12.50", "SGD"));
        assert!(stated("Paid US$12.50", "12.50", "USD"));
        assert!(stated("Paid -USD 12.50", "12.50", "USD"));
        assert!(stated("Paid 12.50 USD", "12.50", "USD"));
        assert!(stated("Paid 12.50 (USD)", "12.50", "USD"));
        assert!(stated("Amount (CAD): 25.00", "25.00", "CAD"));
        assert!(stated("Charged CAD $12.50", "12.50", "CAD"));
        assert!(stated("Charged USD $12.50", "12.50", "USD"));
        assert!(!stated("Charged CAD $12.50", "12.50", "USD"));
        assert!(stated("Amount\nUSD\n12.50", "12.50", "USD"));
        assert!(stated("消费USD12.50", "12.50", "USD"));
        assert!(stated("消费 ¥128.50 元", "128.50", "CNY"));
        assert!(stated("消费 128.50元", "128.50", "CNY"));
        // "US$" is not "S$", and a bare "¥" is yuan or yen.
        assert!(!stated("Paid US$12.50", "12.50", "SGD"));
        assert!(!stated("消费 ¥128.50", "128.50", "CNY"));
        assert!(!stated("Paid USDT 5", "5", "USD"));
        // A bare "$" names no single currency.
        assert!(!stated("Paid $12.50", "12.50", "USD"));
        assert!(!stated("Paid SGD 12.50", "12.50", "USD"));
        assert!(!stated("Paid SGD 12.50", "12.50", "sgd"));
        // A code elsewhere in the email, or one that belongs to the next
        // amount, does not state this amount's currency.
        assert!(!stated("Paid $12.50. Limit USD 5,000", "12.50", "USD"));
        assert!(!stated("Paid 12.50 SGD 75.30", "12.50", "SGD"));
    }

    #[test]
    fn dates_must_be_written_in_the_email() {
        let latest = date("2026-11-25");
        let ok = |text: &str, value: &str| written_date(text, value, latest);
        assert_eq!(ok("on 23 Nov 2026", "2026-11-23"), Some(date("2026-11-23")));
        assert_eq!(ok("on 23 Nov", "2026-11-23"), Some(date("2026-11-23")));
        assert_eq!(ok("on 11/23/2026", "2026-11-23"), Some(date("2026-11-23")));
        assert_eq!(
            ok("Transaction date: 2026-11-23T23:58", "2026-11-23"),
            Some(date("2026-11-23"))
        );
        // Day and month come from one expression, and a written year must match.
        let iso = "Transaction date: 2026-11-23. Amount: USD 12.50";
        assert_eq!(ok(iso, "2026-11-12"), None);
        assert_eq!(ok(iso, "2025-11-23"), None);
        assert_eq!(
            ok("Date: 23-Nov-25", "2025-11-23"),
            Some(date("2025-11-23"))
        );
        assert_eq!(ok("Date: 23-Nov-25", "2026-11-23"), None);
        // An hour or an amount after the date is not its year.
        assert_eq!(
            ok("on 23 Nov 10:15", "2026-11-23"),
            Some(date("2026-11-23"))
        );
        assert_eq!(
            ok("23 Nov 25.00 SGD", "2026-11-23"),
            Some(date("2026-11-23"))
        );
        // A dotted date needs its year, so an amount is not a date.
        assert_eq!(ok("Paid 12.05 SGD", "2026-05-12"), None);
        assert_eq!(ok("on 12.05.2026", "2026-05-12"), Some(date("2026-05-12")));
        // A date the email does not write, a future date, or no date at all.
        assert_eq!(ok("on 22 Nov 2026", "2026-11-23"), None);
        assert_eq!(ok("on 30 Nov 2026", "2026-11-30"), None);
        assert_eq!(ok("on 23 Nov", "yesterday"), None);
    }

    #[test]
    fn phrases_and_card_numbers_must_be_written_in_the_email() {
        let text = "Paid at  Café de Flore with card ending 1234 (account 123-456 789)";
        assert_eq!(
            written_phrase(text, "café de flore").as_deref(),
            Some("café de flore")
        );
        assert_eq!(written_phrase(text, "Starbucks"), None);
        assert_eq!(written_ending(text, "1234").as_deref(), Some("1234"));
        assert_eq!(written_ending(text, "xxxx1234").as_deref(), Some("1234"));
        assert_eq!(written_ending(text, "456 789").as_deref(), Some("456789"));
        assert_eq!(written_ending(text, "5678"), None);
        assert_eq!(written_ending("Ref 12345678", "3456"), None);
    }

    #[test]
    fn account_names_match_cited_numbers() {
        // The first group alone must not match an account ending in it.
        assert!(!names_number("Assets:Checking-123", "123456789"));
        assert!(names_number("Assets:Checking-6789", "123456789"));
        assert!(names_number("Liabilities:Citi-1234", "1234"));
        assert!(!names_number("Liabilities:Visa-91234567", "1234"));
        assert!(!names_number("Assets:Checking", "1234"));
        // Grouped account names compare as one number.
        let grouped = "Assets:Checking-123-45678-9012";
        assert!(!names_number(grouped, "987654329012"));
        assert!(names_number(grouped, "123456789012"));
        assert!(names_number(grouped, "9012"));
        assert!(names_number("Liabilities:Citi-1234", "4111111111111234"));
    }

    #[test]
    fn bounds_work_on_a_huge_plain_text_alternative() {
        // Hundreds of thousands of distinct amounts must not be scanned in full.
        let lines: String = (0..300_000).map(|i| format!("${i}.00\r\n")).collect();
        let raw = format!(
            "Message-ID: <big@x.example>\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\n\
{lines}--b\r\nContent-Type: text/html\r\n\r\n<p>SGD 1.00</p>\r\n--b--\r\n"
        );
        let started = std::time::Instant::now();
        let email = parse(raw.as_bytes()).unwrap();
        assert!(email.body.chars().count() <= MAX_TEXT);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    /// Base64 with 76-character lines, as mail clients write it.
    fn encode_base64(data: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
            for i in 0..4 {
                out.push(if i <= chunk.len() {
                    TABLE[(n >> (18 - 6 * i) & 63) as usize] as char
                } else {
                    '='
                });
            }
        }
        out.as_bytes()
            .chunks(76)
            .map(|line| format!("{}\r\n", std::str::from_utf8(line).unwrap()))
            .collect()
    }

    #[test]
    fn deeply_nested_messages_do_not_overflow_the_stack() {
        // Test threads have 2 MiB stacks, so recursion this deep would abort.
        // Each malformed message must parse or fail quickly and cleanly.
        let nested = "Content-Type: message/rfc822\r\n\r\n".repeat(75_000)
            + "Subject: x\r\n\r\nSGD 1.00\r\n";
        let _ = parse(nested.as_bytes());
        let encoded = format!(
            "Subject: outer\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n\
--b\r\nContent-Type: message/rfc822\r\nContent-Transfer-Encoding: base64\r\n\r\n{}--b--\r\n",
            encode_base64(nested.as_bytes())
        );
        assert!(encoded.len() < crate::inbound::MAX_EMAIL_BYTES);
        // The encoded forward is decoded at most twice, then read as is.
        let _ = parse(encoded.as_bytes());
        // Base64 with spaces inside lines, which mail-parser also decodes.
        let spaced = encoded
            .replace("\r\n", " \r\n")
            .replacen("AAAA", "AA AA", 1000);
        let _ = parse(spaced.as_bytes());
        // A spelling with whitespace inside the header name, which
        // mail-parser still recognises.
        let misspelled = encoded.replace(
            "Content-Transfer-Encoding: base64",
            "Content-Transfer -Encoding: base64",
        );
        let _ = parse(misspelled.as_bytes());
        // A deep tree inside an encoded part that is not message/rfc822.
        let other = encoded.replace(
            "Content-Type: message/rfc822\r\nContent-Transfer-Encoding",
            "Content-Type: application/octet-stream\r\nContent-Transfer-Encoding",
        );
        let _ = parse(other.as_bytes());

        // Many multipart parts whose boundaries never appear.
        let siblings =
            "--a\r\nContent-Type: multipart/mixed; boundary=zz\r\n\r\nx\r\n".repeat(40_000);
        let slow = format!("Content-Type: multipart/mixed; boundary=a\r\n\r\n{siblings}--a--\r\n");
        let started = std::time::Instant::now();
        let _ = parse(slow.as_bytes());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));

        // Overlong or unterminated boundaries and long continuation chains.
        let long = "-".repeat(100_000);
        let dashes = "-".repeat(2_000_000);
        let overlong =
            format!("Content-Type: multipart/mixed; boundary=\"{long}\"\r\n\r\n{dashes}\r\n");
        let started = std::time::Instant::now();
        let _ = parse(overlong.as_bytes());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let unterminated = format!(
            "Content-Type: multipart/mixed; boundary=\"{}\r\n\r\nx\r\n",
            "a".repeat(300)
        );
        let started = std::time::Instant::now();
        let _ = parse(unterminated.as_bytes());
        let segments: String = (0..1000).map(|i| format!("; name*{i}=\"x\"")).collect();
        let continued = format!("Content-Type: text/plain{segments}\r\n\r\nSGD 1.00\r\n");
        let _ = parse(continued.as_bytes());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        // CSS attribute selectors in an HTML body are not continuations.
        let css = "td[style*=\"x\"]{color:red}".repeat(100);
        let styled = format!(
            "Message-ID: <css@x.example>\r\nContent-Type: text/html\r\n\r\n\
<style>{css}</style><p>You spent SGD 3.00</p>\r\n"
        );
        assert!(parse(styled.as_bytes()).unwrap().body.contains("SGD 3.00"));

        // An ordinary forward with an encoded attachment still parses.
        let attached = format!(
            "Message-ID: <fwd@x.example>\r\nSubject: Fwd\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: message/rfc822\r\n\
Content-Transfer-Encoding: base64\r\n\r\n{}--b--\r\n",
            encode_base64(ALERT.as_bytes())
        );
        let email = parse(attached.as_bytes()).unwrap();
        assert!(
            email.body.contains("STARBUCKS VIVOCITY"),
            "{:?}",
            email.body
        );

        // A quoted-printable forward whose alert has a base64 text body.
        let inner = format!(
            "Message-ID: <inner@bank.example>\r\nSubject: Alert\r\nContent-Type: text/plain\r\n\
Content-Transfer-Encoding: base64\r\n\r\n{}",
            encode_base64(b"You spent SGD 12.50 at KOPITIAM.")
        );
        let qp_forward = format!(
            "Message-ID: <fwd2@x.example>\r\nSubject: Fwd\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: message/rfc822\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\r\n{}\r\n--b--\r\n",
            inner.replace('=', "=3D")
        );
        let email = parse(qp_forward.as_bytes()).unwrap();
        assert!(email.body.contains("SGD 12.50"), "{:?}", email.body);

        // A legacy alert without Content-Type is text/plain, base64 or not.
        let legacy = format!(
            "Message-ID: <legacy@bank.example>\r\nSubject: Alert\r\n\
Content-Transfer-Encoding: base64\r\n\r\n{}",
            encode_base64(b"You spent SGD 7.20 at KOPITIAM.")
        );
        let email = parse(legacy.as_bytes()).unwrap();
        assert!(email.body.contains("SGD 7.20"), "{:?}", email.body);
    }

    #[test]
    fn cuts_text_at_line_boundaries() {
        let text = format!("{}\nTotal paid: USD 125.00", "x".repeat(40));
        assert_eq!(truncate_lines(&text, 60), "x".repeat(40));
        assert_eq!(truncate_lines("short", 60), "short");
        assert_eq!(truncate_lines("USD 125.00 total", 8), "USD");
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

        // A placeholder plain-text alternative falls back to the HTML part.
        let alternative = "Message-ID: <a1@bank.example>\r\nSubject: Alert\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\n\
Please view this email in an HTML-capable client.\r\n--b\r\nContent-Type: text/html\r\n\r\n\
<p>You spent <b>SGD 12.50</b> at KOPITIAM.</p>\r\n--b--\r\n";
        let email = parse(alternative.as_bytes()).unwrap();
        let html = email.alternative.unwrap_or_default();
        assert!(html.contains("SGD 12.50"), "{html:?}");

        // Each table row reads as one line.
        let html = "Message-ID: <h1@bank.example>\r\nSubject: Alert\r\nContent-Type: text/html\r\n\r\n\
<table><tr><td>Amount</td><td>SGD 12.50</td><td>23 Nov 2026</td></tr>\
<tr><td>Charged</td><td>SGD</td><td>7.25</td></tr></table>\r\n";
        let email = parse(html.as_bytes()).unwrap();
        let lines: Vec<_> = email.body.lines().filter(|line| !line.is_empty()).collect();
        assert_eq!(
            lines,
            ["Amount SGD 12.50 23 Nov 2026", "Charged SGD 7.25"],
            "{:?}",
            email.body
        );
    }

    fn card_alert_accounts() -> Vec<OpenAccount> {
        vec![
            account("Assets:DBS-Savings", &["SGD"], &[]),
            account("Liabilities:DBS-Card-1234", &["SGD"], &[]),
            account("Expenses:Coffee", &[], &[]),
        ]
    }

    async fn run(
        email: &Email,
        accounts: &[OpenAccount],
        model: &Fixed,
        fields: Value,
    ) -> Result<Statement, Failure> {
        extract(email, accounts, &[], "USD", &Reads(fields), model).await
    }

    fn card_answers() -> Fixed {
        fixed(Answers::from([
            ("account".into(), choice("Liabilities:DBS-Card-1234", &[])),
            (
                "category".into(),
                choice("Expenses:Coffee", &[("Expenses:Coffee", 0.9)]),
            ),
        ]))
    }

    #[tokio::test]
    async fn builds_a_row_from_verified_fields() {
        let email = parse(ALERT.as_bytes()).unwrap();
        let reads = Reads(json!({
            "amount": "12.50", "currency": "SGD", "date": "2026-11-23",
            "payee": "STARBUCKS VIVOCITY", "account_ending": "1234",
        }));
        let model = card_answers();
        let statement = extract(&email, &card_alert_accounts(), &[], "USD", &reads, &model)
            .await
            .unwrap();
        assert_eq!(
            (statement.source.as_str(), statement.currency.as_str()),
            ("Liabilities:DBS-Card-1234", "SGD")
        );
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
        // The cited card matches an account, so "none" is not offered, and
        // every question goes in one call.
        let calls = model.calls.into_inner().unwrap();
        assert_eq!(calls.len(), 1);
        let offered = calls[0]["account"]["enum"].as_array().unwrap();
        assert!(offered.contains(&json!("Liabilities:DBS-Card-1234")));
        assert!(!offered.contains(&json!("none")));
        assert!(calls[0].get("moved").is_none());
    }

    #[tokio::test]
    async fn a_category_naming_the_source_falls_back_to_uncategorized() {
        let email = parse(ALERT.as_bytes()).unwrap();
        let reads = Reads(json!({ "amount": "12.50", "currency": "SGD" }));
        let model = fixed(Answers::from([
            ("account".into(), choice("Liabilities:DBS-Card-1234", &[])),
            (
                "category".into(),
                choice(
                    "Liabilities:DBS-Card-1234",
                    &[("Liabilities:DBS-Card-1234", 0.9)],
                ),
            ),
        ]));
        let statement = extract(&email, &card_alert_accounts(), &[], "USD", &reads, &model)
            .await
            .unwrap();
        assert_eq!(statement.rows[0].account, "Expenses:Uncategorized");
        // So does a category the model is unsure of.
        let unsure = fixed(Answers::from([
            ("account".into(), choice("Liabilities:DBS-Card-1234", &[])),
            (
                "category".into(),
                choice("Expenses:Coffee", &[("Expenses:Coffee", 0.2)]),
            ),
        ]));
        let statement = extract(&email, &card_alert_accounts(), &[], "USD", &reads, &unsure)
            .await
            .unwrap();
        assert_eq!(statement.rows[0].account, "Expenses:Uncategorized");
    }

    #[tokio::test]
    async fn rejects_or_drops_values_the_email_does_not_state() {
        let email = parse(ALERT.as_bytes()).unwrap();
        let accounts = card_alert_accounts();
        let model = card_answers();
        // An amount the email does not state, or a non-transaction, is rejected.
        assert!(matches!(
            run(&email, &accounts, &model, json!({ "amount": "13.00" })).await,
            Err(Failure::Rejected(_))
        ));
        assert!(matches!(
            run(
                &email,
                &accounts,
                &model,
                json!({ "is_transaction": false, "amount": "12.50" })
            )
            .await,
            Err(Failure::Rejected(_))
        ));
        // An invented payee and date are dropped for the subject and sent date.
        let statement = run(
            &email,
            &accounts,
            &model,
            json!({
                "amount": "12.50", "payee": "ACME", "date": "2026-10-01", "account_ending": "1234",
            }),
        )
        .await
        .unwrap();
        let row = &statement.rows[0];
        assert_eq!(
            (row.date, row.description.as_str()),
            (date("2026-11-24"), "Card transaction alert")
        );
    }

    #[tokio::test]
    async fn foreign_purchase_posts_the_settled_amount() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        email.body = "A foreign currency transaction of EUR 50.00 (SGD 75.30) was charged to your card ending 1234 on 21 Nov 2026 at CAFE DE FLORE PARIS.".into();
        let model = card_answers();
        let fields = json!({
            "amount": "50.00", "currency": "EUR", "settled_amount": "75.30",
            "settled_currency": "SGD", "account_ending": "1234", "date": "2026-11-21",
        });
        let accounts = card_alert_accounts();
        let statement = extract(
            &email,
            &accounts,
            &[],
            "USD",
            &Reads(fields.clone()),
            &model,
        )
        .await
        .unwrap();
        assert_eq!(
            (
                statement.currency.as_str(),
                statement.rows[0].amount.as_str()
            ),
            ("SGD", "-75.30")
        );
        // Without a settled amount the SGD card cannot take the EUR charge.
        let mut unsettled = fields;
        unsettled["settled_amount"] = Value::Null;
        let result = extract(&email, &accounts, &[], "USD", &Reads(unsettled), &model).await;
        assert!(matches!(result, Err(Failure::Rejected(_))));
    }

    #[tokio::test]
    async fn a_stated_settled_amount_wins_when_the_account_accepts_it() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        email.body =
            "Purchase EUR 50.00, charged SGD 75.30 to card ending 1234 on 21 Nov 2026.".into();
        let reads = Reads(json!({
            "amount": "50.00", "currency": "EUR", "settled_amount": "75.30",
            "settled_currency": "SGD", "account_ending": "1234",
        }));
        // This card accepts any currency, so EUR 50.00 would also fit.
        let open = vec![
            account("Liabilities:DBS-Card-1234", &[], &[]),
            account("Expenses:Coffee", &[], &[]),
        ];
        let statement = extract(&email, &open, &[], "USD", &reads, &card_answers())
            .await
            .unwrap();
        assert_eq!(
            (
                statement.currency.as_str(),
                statement.rows[0].amount.as_str()
            ),
            ("SGD", "-75.30")
        );
    }

    /// Reads nothing from a body containing `stub`, and `fields` from any other.
    struct StubThenHtml {
        stub: &'static str,
        fields: Value,
    }

    impl Extract for StubThenHtml {
        async fn extract(&self, request: &Request<'_>) -> Result<Answers> {
            if request.text.contains(self.stub) {
                Reads(json!({ "is_transaction": false }))
                    .extract(request)
                    .await
            } else {
                Reads(self.fields.clone()).extract(request).await
            }
        }
    }

    #[tokio::test]
    async fn a_stub_plain_text_falls_back_to_the_html_alternative() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        email.body = "View this alert online at https://bank.example/a/2026 (c) 2026".into();
        email.alternative = Some("You spent SGD 12.50 at KOPITIAM on 23 Nov 2026.".into());
        let reader = StubThenHtml {
            stub: "View this alert online",
            fields: json!({ "amount": "12.50", "currency": "SGD", "payee": "KOPITIAM" }),
        };
        let statement = extract(
            &email,
            &card_alert_accounts(),
            &[],
            "USD",
            &reader,
            &card_answers(),
        )
        .await
        .unwrap();
        assert_eq!(statement.rows[0].description, "KOPITIAM");
    }

    /// Returns `0` exactly as the model's answer.
    struct Raw(Value);

    impl Extract for Raw {
        async fn extract(&self, _: &Request<'_>) -> Result<Answers> {
            Ok(answers(self.0.clone()))
        }
    }

    #[tokio::test]
    async fn a_malformed_answer_is_retried() {
        let email = parse(ALERT.as_bytes()).unwrap();
        let reader = Raw(json!({ "is_transaction": true, "amount": "12.50" }));
        let result = extract(
            &email,
            &card_alert_accounts(),
            &[],
            "USD",
            &reader,
            &card_answers(),
        )
        .await;
        assert!(matches!(result, Err(Failure::Unavailable(_))));
    }

    #[tokio::test]
    async fn a_declined_payment_is_confirmed_before_import() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        email.body = "Your payment of SGD 12.50 at KOPITIAM on 23 Nov 2026 was declined.".into();
        let reads = Reads(json!({ "amount": "12.50", "currency": "SGD", "payee": "KOPITIAM" }));
        let with_moved = |moved: bool| {
            let mut answers = card_answers().answers;
            answers.insert("moved".into(), value(json!(moved)));
            fixed(answers)
        };
        let declined = with_moved(false);
        let result = extract(
            &email,
            &card_alert_accounts(),
            &[],
            "USD",
            &reads,
            &declined,
        )
        .await;
        assert!(matches!(result, Err(Failure::Rejected(_))));
        assert!(declined.calls.lock().unwrap()[0].get("moved").is_some());
        // A refund for a cancelled order did move money.
        email.body =
            "Order cancelled. A refund of SGD 12.50 from KOPITIAM was made on 23 Nov 2026.".into();
        let refund = Reads(json!({
            "direction": "in", "amount": "12.50", "currency": "SGD", "payee": "KOPITIAM",
        }));
        let statement = extract(
            &email,
            &card_alert_accounts(),
            &[],
            "USD",
            &refund,
            &with_moved(true),
        )
        .await
        .unwrap();
        assert_eq!(statement.rows[0].amount, "12.50");
        // Without the decline words, no question is asked.
        let plain = card_answers();
        email.body = "You spent SGD 12.50 at KOPITIAM on 23 Nov 2026.".into();
        extract(&email, &card_alert_accounts(), &[], "USD", &reads, &plain)
            .await
            .unwrap();
        assert!(
            plain
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|q| q.get("moved").is_none())
        );
    }

    #[tokio::test]
    async fn a_settled_amount_needs_its_own_currency() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        // The model reads 75.30 as an SGD settled amount, but no currency is
        // written with it, so the SGD card is not charged 75.30.
        email.body = "Paid EUR 50.00 (75.30) at CAFE DE FLORE on 21 Nov 2026.".into();
        let reads = Reads(json!({
            "amount": "50.00", "currency": "EUR", "settled_amount": "75.30",
            "settled_currency": "SGD", "payee": "CAFE DE FLORE",
        }));
        let result = extract(
            &email,
            &card_alert_accounts(),
            &[],
            "USD",
            &reads,
            &card_answers(),
        )
        .await;
        assert!(matches!(result, Err(Failure::Rejected(_))));
    }

    #[tokio::test]
    async fn a_currency_written_with_the_amount_is_never_replaced() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        email.subject = String::new();
        email.body =
            "Your card ending 1234 was charged CAD $12.50 at TIM HORTONS on 23 Nov 2026.".into();
        let card = vec![account("Liabilities:Card-1234", &["USD", "CAD"], &[])];
        let closed = ["Assets:Checking".to_owned()];
        let decide = || {
            fixed(Answers::from([(
                "category".into(),
                choice("Expenses:Dining", &[("Expenses:Dining", 0.9)]),
            )]))
        };
        // Read as CAD, it is posted in CAD.
        let reads =
            Reads(json!({ "amount": "12.50", "currency": "CAD", "account_ending": "1234" }));
        let statement = extract(&email, &card, &closed, "USD", &reads, &decide())
            .await
            .unwrap();
        assert_eq!(statement.currency, "CAD");
        // With the currency unread, it is rejected rather than posted in USD.
        let unread = Reads(json!({ "amount": "12.50", "account_ending": "1234" }));
        let usd = vec![account("Liabilities:Card-1234", &["USD"], &[])];
        let result = extract(&email, &usd, &closed, "USD", &unread, &decide()).await;
        assert!(matches!(result, Err(Failure::Rejected(_))));
    }

    #[tokio::test]
    async fn categories_are_offered_in_the_posted_currency() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        email.body =
            "Your card ending 1234 was charged EUR 50.00 (SGD 75.30) at CAFE DE FLORE on 21 Nov 2026."
                .into();
        let ledger = vec![
            account("Liabilities:Card-1234", &["SGD"], &[]),
            account("Expenses:Cafes", &["SGD"], &[]),
        ];
        let closed = ["Assets:Checking".to_owned()];
        let reads = Reads(json!({
            "amount": "50.00", "currency": "EUR", "settled_amount": "75.30",
            "settled_currency": "SGD", "account_ending": "1234",
        }));
        let model = fixed(Answers::from([(
            "category".into(),
            choice("Expenses:Cafes", &[("Expenses:Cafes", 0.9)]),
        )]));
        let statement = extract(&email, &ledger, &closed, "USD", &reads, &model)
            .await
            .unwrap();
        assert_eq!(
            (
                statement.currency.as_str(),
                statement.rows[0].account.as_str()
            ),
            ("SGD", "Expenses:Cafes")
        );
    }

    #[tokio::test]
    async fn an_incoming_transfer_asks_for_the_credited_account() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        email.body = "Your account ending 8890 was credited SGD 100.00 on 23 Nov 2026.".into();
        let reads = Reads(json!({ "direction": "in", "amount": "100.00", "currency": "SGD" }));
        let model = fixed(Answers::from([
            ("account".into(), choice("Assets:DBS-Savings", &[])),
            (
                "category".into(),
                choice("Income:Uncategorized", &[("Income:Uncategorized", 0.9)]),
            ),
        ]));
        extract(&email, &card_alert_accounts(), &[], "USD", &reads, &model)
            .await
            .unwrap();
        let calls = model.calls.into_inner().unwrap();
        let asked = calls[0]["account"]["instructions"].as_str().unwrap();
        assert!(asked.contains("money into") && asked.contains("reports as credited"));
    }

    #[tokio::test]
    async fn an_unstated_currency_takes_the_account_currency() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        email.subject = String::new();
        email.body = "You paid $8.00 to Grab on 2026-11-23.".into();
        // The model guesses USD for "$", which the email does not state.
        let reads = Reads(json!({ "amount": "8.00", "currency": "USD", "payee": "Grab" }));
        let model = fixed(Answers::from([
            ("account".into(), choice("Assets:Wallet", &[])),
            (
                "category".into(),
                choice("Expenses:Transport", &[("Expenses:Transport", 0.9)]),
            ),
        ]));
        let wallet = vec![account("Assets:Wallet", &[], &["SGD"])];
        let statement = extract(&email, &wallet, &[], "USD", &reads, &model)
            .await
            .unwrap();
        assert_eq!(
            (statement.source.as_str(), statement.currency.as_str()),
            ("Assets:Wallet", "SGD")
        );
        assert_eq!(statement.rows[0].description, "Grab");
        // A ledger with only a bank account is still offered a credit card.
        let calls = model.calls.into_inner().unwrap();
        let offered = calls[0]["account"]["enum"].as_array().unwrap();
        assert!(
            offered.contains(&json!("Liabilities:CreditCard")) && !offered.contains(&json!("none"))
        );
    }

    #[tokio::test]
    async fn an_unknown_card_can_be_refused() {
        let mut email = parse(ALERT.as_bytes()).unwrap();
        email.body = "Your card ending 5678 was charged SGD 12.50 at GRAB on 23 Nov 2026.".into();
        let reads =
            Reads(json!({ "amount": "12.50", "currency": "SGD", "account_ending": "5678" }));
        let model = fixed(Answers::from([("account".into(), choice("none", &[]))]));
        let result = extract(&email, &card_alert_accounts(), &[], "USD", &reads, &model).await;
        assert!(matches!(result, Err(Failure::Rejected(_))));
        let calls = model.calls.into_inner().unwrap();
        assert!(
            calls[0]["account"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("none"))
        );
    }

    #[test]
    fn limit_keeps_the_account_matching_a_cited_card_number() {
        // Every account shares the issuer name, which the email also mentions.
        let shared: BTreeMap<String, String> = (0..60)
            .map(|i| (format!("Assets:Bank-{i:02}"), String::new()))
            .chain([("Liabilities:Bank-Visa-91234".into(), String::new())])
            .collect();
        let kept = limit(
            shared,
            "Bank purchase on card ending 1234",
            Some("1234"),
            |_| 1,
        );
        assert!(kept.contains_key("Liabilities:Bank-Visa-91234"));
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
        let kept = limit(options, "Paid at BAKERY", None, |name| {
            if name.starts_with("Income:") { 4 } else { 1 }
        });
        assert_eq!(kept.len(), MAX_CHOICES);
        assert!(
            kept.contains_key("Expenses:Uncategorized")
                && kept.contains_key("Expenses:Zeta-Bakery")
        );
        assert!(!kept.contains_key("Income:Salary"));
    }
}
