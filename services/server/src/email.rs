//! Turns a forwarded transaction email into a one-row statement.
//!
//! Deterministic code finds every amount, date, and payee phrase written in
//! the email. A decision model then chooses among those candidates and the
//! ledger's accounts, so it never invents a value that the email does not
//! contain.

use std::{collections::BTreeMap, sync::LazyLock};

use anyhow::{Context, Result};
use chrono::{Datelike, Days, NaiveDate};
use mail_parser::{MessageParser, MessagePart, MessageRef, PartKind, html_to_text};
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
    Email {
        id,
        from: clean(truncate(&from, MAX_HEADER)),
        subject: strip_forward_prefix(&clean(truncate(
            message.subject().unwrap_or_default(),
            MAX_HEADER,
        )))
        .to_owned(),
        sent,
        // Bound the text before cleaning it, leaving room for whitespace that
        // cleaning removes and for a currency code just past the limit.
        body: truncate_lines(
            &join_split_amounts(&clean(truncate_lines(&body_text(message), MAX_TEXT * 4))),
            MAX_TEXT,
        )
        .to_owned(),
    }
}

static BLOCK_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<(/?(?:tr|p|div|br|li|h[1-6]|table|section|header|footer)\b)").unwrap()
});

static CELL_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<(/?t[dh]\b)").unwrap());

/// The first text body. HTML gets a line break at each row and block tag and
/// a space between table cells, so "Total" and "S$17.10" do not run together
/// while "SGD" and "12.50" in one row stay on one line.
fn body_text(message: MessageRef) -> String {
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
        Some(part) if is_html(&part) => as_text(part),
        Some(part) => {
            let text = as_text(part);
            // A plain-text alternative such as "View this email in HTML"
            // carries no amount; read the HTML alternative instead.
            match html_part {
                Some(html)
                    if amounts(truncate_lines(
                        &join_split_amounts(&clean(truncate_lines(&text, MAX_TEXT * 4))),
                        MAX_TEXT,
                    ))
                    .is_empty() =>
                {
                    as_text(html)
                }
                _ => text,
            }
        }
        None => html_part.map(as_text).unwrap_or_default(),
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

/// Active ISO 4217 currency codes, plus RMB for CNY. Codes that are common
/// English words or abbreviations in capitals (ALL, BOB, CUP, CVE, GEL, MAD,
/// MOP, PEN, SOS, TOP) and fund codes are left out, so "ALL 3 ITEMS" is not
/// read as an amount.
const CODES: &str = "AED|AFN|AMD|AOA|ARS|AUD|AWG|AZN|BAM|BBD|BDT|BGN|BHD|BIF|BMD|BND|BRL|BSD|BTN|BWP|BYN|BZD|CAD|CDF|CHF|CLP|CNY|COP|CRC|CZK|DJF|DKK|DOP|DZD|EGP|ERN|ETB|EUR|FJD|FKP|GBP|GHS|GIP|GMD|GNF|GTQ|GYD|HKD|HNL|HTG|HUF|IDR|ILS|INR|IQD|IRR|ISK|JMD|JOD|JPY|KES|KGS|KHR|KMF|KPW|KRW|KWD|KYD|KZT|LAK|LBP|LKR|LRD|LSL|LYD|MDL|MGA|MKD|MMK|MNT|MRU|MUR|MVR|MWK|MXN|MYR|MZN|NAD|NGN|NIO|NOK|NPR|NZD|OMR|PAB|PGK|PHP|PKR|PLN|PYG|QAR|RON|RSD|RUB|RWF|SAR|SBD|SCR|SDG|SEK|SGD|SHP|SLE|SRD|SSP|STN|SVC|SYP|SZL|THB|TJS|TMT|TND|TRY|TTD|TWD|TZS|UAH|UGX|USD|UYU|UZS|VED|VES|VND|VUV|WST|XAF|XCD|XCG|XOF|XPF|YER|ZAR|ZMW|ZWG|RMB";
const NUMBER: &str = r"[0-9]{1,3}(?:,[0-9]{3})+(?:\.[0-9]{1,3})?|[0-9]+(?:\.[0-9]{1,3})?";

/// Currencies with three minor-unit digits, such as "KWD 12.345". Elsewhere
/// "EUR 1.234" is a grouped thousand, so three decimals are skipped.
const THREE_DECIMALS: [&str; 7] = ["BHD", "IQD", "JOD", "KWD", "LYD", "OMR", "TND"];

static CODE_BEFORE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"\b({CODES}) ?(?:[-−+] ?)?$")).unwrap());

/// A code right after an amount, bare or alone in parentheses as in "(USD)".
/// "(USD 18.50)" is a second amount, not an annotation.
/// An amount at the start of the text, with an optional sign and symbol.
static NEXT_AMOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"^ ?(?:[-−+] ?)?(?:(?:{SYMBOLS}) ?)?(?:[-−+] ?)?[0-9][0-9,]*(?:\.[0-9]+)?"
    ))
    .unwrap()
});

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

/// Whether `after` starts with a month name or abbreviation, as the " Nov",
/// "-Nov", or "rd Nov" after a day number in "23 Nov", "23-Nov", or "23rd Nov".
fn starts_with_month(after: &str) -> bool {
    let lower = after.to_ascii_lowercase();
    let rest = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|suffix| lower.strip_prefix(suffix))
        .unwrap_or(&lower);
    let word: String = rest
        .trim_start_matches([' ', '-'])
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_lowercase();
    word.len() >= 3 && MONTHS.iter().any(|month| month.starts_with(&word))
}

/// Whether a number of at most two digits is the day of a "23 Nov" date.
fn is_day_of_month(number: &str, after: &str) -> bool {
    number.len() <= 2 && starts_with_month(after)
}

/// Whether text right after a number continues it as part of a date, time,
/// or longer number, as in the "/23/2026" of "11/23/2026", the ".2026" of
/// "23.11.2026", or the ":15" of "10:15".
fn continues_number(after: &str) -> bool {
    let mut chars = after.chars();
    match chars.next() {
        Some(c) if c.is_ascii_digit() => true,
        Some('/' | ':' | '-' | '.' | ',') => chars.next().is_some_and(|c| c.is_ascii_digit()),
        _ => false,
    }
}

static CODE_AFTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^ ?(?:\(({CODES})\)|({CODES})\b)")).unwrap());

/// Two-letter prefixes may be spaced from "$", as in "US $19.99". One-letter
/// prefixes must be attached, since "PLAN A $5" is not Australian dollars.
const SYMBOLS: &str = r"\b(?:US|CA|AU|HK|NZ|SG) ?\$|S\$|A\$|C\$|\$|€|£|¥|₹";

/// A currency symbol or code followed by a number, as in "S$12.50" or "SGD 12.50".
static PREFIX_AMOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?:(?P<symbol>{SYMBOLS})|\b(?P<code>{CODES})) ?(?:[-−+] ?)?(?P<number>{NUMBER})"
    ))
    .unwrap()
});

/// A number followed by a currency code, as in "12.50 SGD". Matched separately
/// from [`PREFIX_AMOUNT`] so that in "Ref 123 SGD 12.50" both readings are
/// offered and the model chooses.
static SUFFIX_AMOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"\b(?P<number2>{NUMBER}) ?(?P<code2>{CODES})\b")).unwrap()
});

/// A line ending in a currency marker, such as "Amount: USD", before a line
/// starting with a number,
/// or a number before a line holding only a code, as table cells often render.
static SPLIT_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?m)(\b(?:{CODES})|{SYMBOLS})[ \t]*\n+((?:[-−+] ?)?(?:(?:{SYMBOLS}) ?(?:[-−+] ?)?)?[0-9])"
    ))
    .unwrap()
});
static SPLIT_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?m)^((?:[-−+] ?)?(?:(?:{SYMBOLS}) ?)?(?:[-−+] ?)?[0-9][0-9,.]*)\n+((?:{CODES})$)"
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
        // Part of a date or time, as the "11" of "CAD 11/23/2026". A day such
        // as the "5" of "SGD 5 May 12" stays a candidate for the model.
        || continues_number(after)
        || (decimal_mark(next) && digit(next2))
        || (decimal_mark(prev) && digit(prev2))
        || (group_space(next)
            && !number.contains(['.', ','])
            && number.len() <= 3
            && group(&after[next.map_or(0, char::len_utf8)..]))
        // A digit group before a space-grouped thousand must itself be a plain
        // group of one to three digits, not the end of a date such as the
        // "2026-11-23" of "2026-11-23 250.00 SGD".
        || (group_space(prev)
            && digit(prev2)
            && whole.len() == 3
            && !whole.contains(',')
            && {
                // The digits right before the separator, which may touch a
                // currency marker as in "CHF1’234.50", unless a date or time
                // separator precedes them.
                let head = &before[..before.len() - prev.map_or(0, char::len_utf8)];
                let digits = head.len() - head.trim_end_matches(|c: char| c.is_ascii_digit()).len();
                // A separator counts as a date or time only after a digit, as in
                // "2026-11-23"; after a currency marker, as in "CHF-1’234.50",
                // it is a sign.
                let mut lead = head[..head.len() - digits].chars().rev();
                let in_date = matches!(lead.next(), Some('-' | '/' | '.' | ':' | ','))
                    && lead.next().is_some_and(|c| c.is_ascii_digit());
                (1..=3).contains(&digits) && !in_date
            })
}

/// Every distinct amount written with a currency symbol or ISO code.
pub fn amounts(text: &str) -> Vec<(Money, String)> {
    let mut found: Vec<(Money, String, u8)> = Vec::new();
    let mut claimed = std::collections::HashSet::new();
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
        // A number the prefix pass already read, or found ambiguous, is not
        // read again as "number code". An unknown symbol such as "R$" in
        // "R$ 10.00 BRL" claims nothing, so the code still applies.
        let key = (span.start(), span.end());
        if caps.name("number2").is_some() && claimed.contains(&key) {
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
        let trailing_code = caps
            .name("symbol")
            .and(CODE_AFTER.captures(after).and_then(|code| {
                // A bare code followed by a whole amount, as in "USD $25.00 SGD
                // 33.50" or "SGD $33.50", introduces the next amount rather than
                // annotating this one. A date such as "CAD 11/23/2026" does not.
                let rest = &after[code.get(0).unwrap().end()..];
                if code.get(2).is_some()
                    && NEXT_AMOUNT.find(rest).is_some_and(|next| {
                        let tail = &rest[next.end()..];
                        let digits = next
                            .as_str()
                            .trim_start_matches(|c: char| !c.is_ascii_digit());
                        !continues_number(tail) && !is_day_of_month(digits, tail)
                    })
                {
                    return None;
                }
                Some(code.get(1).or(code.get(2)).unwrap().as_str())
            }));
        // Codes on both sides that disagree, as in "CAD $25.00 USD", are ambiguous.
        if let (Some(a), Some(b)) = (leading_code, trailing_code)
            && a != b
        {
            claimed.insert(key);
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
        if caps.name("number").is_some() {
            claimed.insert(key);
        }
        let symbol = caps.name("symbol").map(|m| m.as_str().replace(' ', ""));
        let currency = match symbol.as_deref() {
            Some("$" | "¥") if code.is_some() => code,
            Some("US$") => Some("USD"),
            Some("SG$") => Some("SGD"),
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
        let decimals = span.as_str().split_once('.').map_or(0, |(_, f)| f.len());
        if decimals == 3 && !currency.is_some_and(|code| THREE_DECIMALS.contains(&code)) {
            continue;
        }
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
        // A number may follow its label on the same line, or, after an
        // explicit identifier label such as "Card number" or "Account:", on
        // one of the next two lines, as separate HTML blocks render it.
        r"(?i)(?:\bending(?: in| with)?\s*|\bends (?:in|with)\s*|[x*•]{2,}[ -]?)(?P<tail>[0-9]{3,4})\b|\b(?:account|acct|a/c|card)(?: (?:no\.?|num\.?|number))?[ \t]*[:#.]?[ \t]*\(?(?:[x*•]+[ -]?)?(?P<number>[0-9]{3,19}(?:-[0-9]{1,19}| [0-9]{3,19}){0,4})\b|\b(?:account|acct|a/c|card)(?: (?:no\.?|num\.?|number)[ \t]*[:#.]?|[ \t]*[:#])[ \t]*(?:\r?\n[ \t]*){1,2}(?:[x*•]+[ -]?)?(?P<below>[0-9]{3,19}(?:-[0-9]{1,19}| [0-9]{3,19}){0,4})\b",
    )
    .unwrap()
});

/// The card and account numbers `text` cites, as in "ending 1234",
/// "XXXX1234", "Account number: 123-456789", or "Card: 1234". Grouping
/// spaces and hyphens are removed, so a group is never matched alone.
fn cited_numbers(text: &str) -> Vec<String> {
    CARD_DIGITS
        .captures_iter(text)
        .filter_map(|caps| {
            caps.name("tail")
                .or(caps.name("number"))
                .or(caps.name("below"))
        })
        .filter_map(|number| {
            // The integer part of an amount, such as the "100" of "100.00 USD"
            // or the "1000" of "Card: 5678 1000.00", is not part of the number.
            let after = text[number.end()..].as_bytes();
            let into_amount = matches!(after.first(), Some(b'.' | b','))
                && after.get(1).is_some_and(u8::is_ascii_digit);
            let digits = if into_amount {
                &number.as_str()[..number.as_str().rfind([' ', '-'])?]
            } else {
                number.as_str()
            };
            Some(digits.replace([' ', '-'], ""))
        })
        .collect()
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

const MONTH: &str = r"(?P<month>(?i:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-zA-Z]*)\.?";

static DATES: LazyLock<[Regex; 5]> = LazyLock::new(|| {
    [
        // Also the date of an ISO timestamp such as 2026-11-23T23:58:00+08:00.
        // Year first, as in 2026-11-23, 2026/11/23, or 2026-11-23T23:58.
        Regex::new(r"\b(?P<year>[0-9]{4})[-/.](?P<m>[0-9]{1,2})[-/.](?P<day>[0-9]{1,2})(?:T|\b)")
            .unwrap(),
        Regex::new(&format!(
            r"\b(?P<day>[0-9]{{1,2}})(?:st|nd|rd|th)?[ -]{MONTH}(?:[ ,-]+(?P<year>[0-9]{{4}}|[0-9]{{2}}))?\b"
        ))
        .unwrap(),
        Regex::new(&format!(
            r"\b{MONTH} (?P<day>[0-9]{{1,2}})(?:st|nd|rd|th)?(?:,? (?P<year>[0-9]{{4}}|[0-9]{{2}}))?\b"
        ))
        .unwrap(),
        Regex::new(
            r"\b(?P<a>[0-9]{1,2})(?P<s1>[/.-])(?P<b>[0-9]{1,2})(?P<s2>[/.-])(?P<year>[0-9]{4}|[0-9]{2})\b",
        )
        .unwrap(),
        // A yearless "11/24" right after a date label, or on the next line,
        // so fractions and ratios elsewhere are not read as dates.
        Regex::new(
            r"(?i)(?:\b(?:date|dated)\b[ \t]*:?[ \t]*(?:\r?\n[ \t]*){0,2}|\bon\b[ \t]*:?[ \t]*)(?P<a>[0-9]{1,2})/(?P<b>[0-9]{1,2})\b",
        )
        .unwrap(),
    ]
});

/// Every distinct plausible date written in the text, within about a year
/// before `anchor`. Slash dates yield both day-first and month-first readings.
pub fn dates(text: &str, anchor: NaiveDate) -> Vec<(NaiveDate, String)> {
    let earliest = anchor - Days::new(400);
    let first = NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();
    let latest = anchor + Days::new(1);
    let mut found: Vec<(NaiveDate, String)> = Vec::new();
    for (kind, pattern) in DATES.iter().enumerate() {
        for caps in pattern.captures_iter(text) {
            let whole = caps.get(0).unwrap();
            let number = |name| caps.name(name).and_then(|m| m.as_str().parse::<u32>().ok());
            // A "year" that runs on is an hour or an amount, as in "23 Nov
            // 10:15", "23 Nov 25.00 SGD", or "23 Nov 1250.00 SGD", so the date
            // stays yearless.
            let year = number("year")
                .filter(|_| {
                    caps.name("year").is_none_or(|year| {
                        let rest = text[year.end()..].as_bytes();
                        !(matches!(rest.first(), Some(b':' | b'.' | b','))
                            && rest.get(1).is_some_and(u8::is_ascii_digit))
                    })
                })
                .map(|year| if year < 100 { year + 2000 } else { year });
            let mut readings: Vec<(Option<u32>, u32, u32)> = match kind {
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
                    if let (Some(s1), Some(s2)) = (caps.name("s1"), caps.name("s2"))
                        && s1.as_str() != s2.as_str()
                    {
                        continue;
                    }
                    // The yearless form must not be the start of a full date.
                    if caps.name("s1").is_none() && text[whole.end()..].starts_with('/') {
                        continue;
                    }
                    vec![(year, b, a), (year, a, b)]
                }
            };
            // In "23 Nov 25 SGD" or "23 Nov 1250 SGD" the number may be the year
            // or the amount, so offer the yearless reading too.
            if caps
                .name("year")
                .is_some_and(|year| CODE_AFTER.is_match(&text[year.end()..]))
            {
                let yearless: Vec<_> = readings.iter().map(|&(_, m, d)| (None, m, d)).collect();
                readings.extend(yearless);
            }
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
                // An explicit year is kept back to 1900, so an old alert forwarded
                // today keeps its date. An inferred year stays within the window.
                let start = if year.is_some() { first } else { earliest };
                let Some(date) = date.filter(|date| (start..=latest).contains(date)) else {
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
        r"(?m)\b(?i:at|to|from|merchant(?: name)?:|payee:|paid to|recipient:|description:|transaction details:)\s+(?P<payee>[\p{Lu}0-9][\p{L}\p{M}0-9&'’*./ -]{1,60}?)(?:\s+(?i:on|for|via|using|with|ending|at|from|is|has|was|of)\b|[,;:!()\n]|\.\s|\.?$)",
    )
    .unwrap()
});

/// Phrases that name the other party, such as the text after "at" or "to".
pub fn payees(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for caps in PAYEE.captures_iter(text) {
        let payee = caps["payee"].trim().trim_end_matches(['.', '-', '/', ' ']);
        if payee.len() >= 2
            && payee.chars().any(char::is_alphabetic)
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
    // The ledger may lack the card or account the email names. When the
    // email cites a number, such as "ending 5678", that no account name ends
    // with, the model may answer "none". Unnumbered accounts stay on offer,
    // so "Liabilities:Amex" can still match an Amex alert by name.
    let cited = cited_numbers(&text);
    // Any unmatched number counts: a transfer can cite both accounts.
    let offer_none = cited
        .iter()
        .any(|number| !sources.keys().any(|name| names_number(name, number)));
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
                "None of these: the account this transaction moves money in or out of is not listed.".into(),
            );
        }
        questions.insert("account".into(), json!({
            "type": "choice",
            "instructions": "Which of the user's accounts did the transaction in `email` move money in or out of? For a transfer between two accounts, choose the one the email reports as debited or charged if it is listed. Match bank names and card or account numbers where possible.",
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
            // Rejection reasons reach the relay or a bounce, so they name no
            // account and no configured currency.
            rejected("The email has no settled amount in a currency the account accepts.")
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
        return Err(rejected(
            "The account does not accept the transaction's currency.",
        ));
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
                Err(rejected(
                    "No category fits, and the fallback category is closed or does not accept the transaction's currency.",
                ))
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
    let cited = cited_numbers(text);
    let text = text.to_lowercase();
    // A numeric part ending in a cited card number is the strongest match,
    // then any name part that appears in the email.
    let card = |name: &str| cited.iter().any(|number| names_number(name, number));
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
                "1990-01-01",
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
        // A yearless numeric date after a label, but not a bare fraction.
        let anchor_25 = date("2026-11-25");
        let found: Vec<_> = dates("Transaction date: 11/24", anchor_25)
            .into_iter()
            .map(|(date, _)| date.to_string())
            .collect();
        assert!(found.contains(&"2026-11-24".to_string()), "{found:?}");
        assert!(dates("Add 1/2 cup and 3/4 spoon", anchor_25).is_empty());
        let labelled: Vec<_> = dates("Purchased on: 11/24", anchor_25)
            .into_iter()
            .map(|(date, _)| date.to_string())
            .collect();
        assert!(labelled.contains(&"2026-11-24".to_string()), "{labelled:?}");
        assert!(dates("Paid on\n1/2", anchor_25).is_empty());
        let next_line: Vec<_> = dates("Transaction date:\n11/24", anchor_25)
            .into_iter()
            .map(|(date, _)| date.to_string())
            .collect();
        assert!(
            next_line.contains(&"2026-11-24".to_string()),
            "{next_line:?}"
        );
        // One full date; the yearless pattern does not add a second reading.
        assert_eq!(dates("Paid on 11/24/2026", anchor_25).len(), 1);
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
        // An explicit year older than the window is kept for old forwarded alerts.
        assert_eq!(dates("Paid on 2024-03-05", anchor)[0].0, date("2024-03-05"));
        // Two-digit years after named months, but not an hour.
        assert_eq!(dates("Date: 23-Nov-25", anchor)[0].0, date("2025-11-23"));
        assert_eq!(dates("Nov 3, 25", anchor)[0].0, date("2025-11-03"));
        assert_eq!(dates("on 23 Nov 10:15", anchor)[0].0, date("2026-11-23"));
        assert_eq!(dates("23 Nov 25.00 SGD", anchor)[0].0, date("2026-11-23"));
        assert_eq!(dates("23 Nov 1250.00 SGD", anchor)[0].0, date("2026-11-23"));
        // A two-digit "year" that is also an amount keeps both readings.
        let both: Vec<_> = dates("23 Nov 25 SGD", anchor)
            .into_iter()
            .map(|(d, _)| d)
            .collect();
        assert!(
            both.contains(&date("2025-11-23")) && both.contains(&date("2026-11-23")),
            "{both:?}"
        );
        let four: Vec<_> = dates("23 Nov 1250 SGD", anchor)
            .into_iter()
            .map(|(d, _)| d)
            .collect();
        assert!(four.contains(&date("2026-11-23")), "{four:?}");
        // A yearless date after the anchor belongs to the previous year.
        assert_eq!(dates("on 30 Dec", anchor)[0].0, date("2025-12-30"));
    }

    #[test]
    fn rejects_partial_and_non_ascii_numbers() {
        assert!(amounts("€12,34 and 1.234,56 EUR and S$12.505").is_empty());
        assert!(amounts("EUR 1 234.56 and 1\u{202f}234 USD and CHF 1'234.50").is_empty());
        assert_eq!(amounts("SGD 12 on 3 Nov").len(), 1);
        assert!(amounts("€1’234.56 and R$10 and NT$300 and €12.٣٤").is_empty());
        // An explicit plus sign, as in a credit alert with a balance.
        let found: Vec<_> =
            amounts("Amount credited: USD +100.00. Available balance: USD 1,400.00")
                .into_iter()
                .map(|(money, _)| money.number)
                .collect();
        assert_eq!(found, ["100.00", "1400.00"]);
        // A signed amount split from its code by HTML blocks.
        let split =
            join_split_amounts("Amount credited\nUSD\n\n+100.00\nAvailable balance: USD 1,400.00");
        let found: Vec<_> = amounts(&split)
            .into_iter()
            .map(|(money, _)| money.number)
            .collect();
        assert_eq!(found, ["100.00", "1400.00"], "{split:?}");
        // A code split from a symbol amount keeps its currency.
        for split in ["USD\n\n+$100.00", "USD\n$100.00", "+$100.00\nUSD"] {
            let found = amounts(&join_split_amounts(split));
            assert_eq!(found.len(), 1, "{split:?}");
            assert_eq!(found[0].0.currency.as_deref(), Some("USD"), "{split:?}");
        }
        // A code before a date annotates the amount; a date is not an amount.
        for dotted in ["Total $25.00 CAD 23.11.2026", "Total $25.00 CAD 23-11-2026"] {
            let found = amounts(dotted);
            assert_eq!(found.len(), 1, "{dotted}");
            assert_eq!(found[0].0.currency.as_deref(), Some("CAD"), "{dotted}");
        }
        let found: Vec<_> = amounts("Total $25.00 CAD 11/23/2026")
            .into_iter()
            .map(|(m, _)| (m.number, m.currency.unwrap()))
            .collect();
        assert_eq!(found, [("25.00".into(), "CAD".into())]);
        let found: Vec<_> = amounts("USD $25.00 SGD $33.50")
            .into_iter()
            .map(|(m, _)| (m.number, m.currency.unwrap()))
            .collect();
        assert_eq!(
            found,
            [
                ("25.00".into(), "USD".into()),
                ("33.50".into(), "SGD".into())
            ]
        );
        // A day-and-month date after the code is not the next amount, so the
        // amount keeps CAD; the day stays an extra candidate for the model.
        for dated in [
            "Total $25.00 CAD 23 Nov",
            "Total $25.00 CAD 23-Nov",
            "Total $25.00 CAD 23rd Nov",
        ] {
            let found: Vec<_> = amounts(dated)
                .into_iter()
                .map(|(m, _)| (m.number, m.currency))
                .collect();
            assert_eq!(found[0], ("25.00".into(), Some("CAD".into())), "{dated}");
        }
        // A marked amount before a month name stays a candidate.
        assert_eq!(amounts("Paid SGD 5 May 12 at Cafe")[0].0.number, "5");
        // A glued currency marker does not hide a thousands group, with or
        // without a sign.
        for grouped in [
            "Total CHF1’234.50 CHF",
            "Refund: CHF-1’234.50 CHF",
            "Refund: CHF−1’234.50 CHF",
        ] {
            assert!(amounts(grouped).is_empty(), "{grouped}");
        }
        // The end of an ISO date is not a thousands group.
        let found = amounts("Paid on 2026-11-23 250.00 SGD");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.number, "250.00");
        // A code that starts the next cell's amount is not a trailing code.
        let found: Vec<_> = amounts("USD $25.00 SGD 33.50")
            .into_iter()
            .map(|(m, _)| (m.number, m.currency.unwrap()))
            .collect();
        assert_eq!(
            found,
            [
                ("25.00".into(), "USD".into()),
                ("33.50".into(), "SGD".into())
            ]
        );
        // An unknown symbol does not hide a code after the amount.
        let found = amounts("Total R$ 10.00 BRL");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.currency.as_deref(), Some("BRL"));
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
        // A label before the currency on its line.
        let labelled = join_split_amounts("Amount: USD\n25.00\nAvailable balance: USD 500.00");
        let found: Vec<_> = amounts(&labelled)
            .into_iter()
            .map(|(m, _)| m.number)
            .collect();
        assert_eq!(found, ["25.00", "500.00"], "{labelled:?}");
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
        let currencies: Vec<_> = amounts("Total paid: US $19.99, HK $5, PLN 49.99, 12.00 PLN")
            .into_iter()
            .map(|(money, _)| money.currency.unwrap())
            .collect();
        assert_eq!(currencies, ["USD", "HKD", "PLN", "PLN"]);
        // English words in capitals are not currency codes, and a spaced
        // one-letter prefix is a bare dollar.
        let found = amounts("ALL 3 ITEMS. TOP 5 DEALS. PLAN A $5 on BUS $6");
        let found: Vec<_> = found
            .into_iter()
            .map(|(m, _)| (m.number, m.currency))
            .collect();
        assert_eq!(found, [("5".into(), None), ("6".into(), None)]);
        assert_eq!(
            amounts("Refund CAD −$25.00 and CAD - $26.00")
                .into_iter()
                .map(|(m, _)| m.currency.unwrap())
                .collect::<Vec<_>>(),
            ["CAD", "CAD"]
        );
        // A sign between the code and "$" keeps the code; three decimals only
        // for currencies that have them.
        let found: Vec<_> = amounts("Refund CAD -$25.00, KWD 12.345, 1.500 OMR, EUR 1.234, $2.345")
            .into_iter()
            .map(|(m, _)| (m.number, m.currency.unwrap()))
            .collect();
        assert_eq!(
            found,
            [
                ("25.00".into(), "CAD".into()),
                ("12.345".into(), "KWD".into()),
                ("1.500".into(), "OMR".into())
            ]
        );
        let found = amounts("Purchase amount: $25.00 (USD). Settled amount: SGD 33.50");
        assert_eq!(found[0].0.currency.as_deref(), Some("USD"));
        // A parenthesised second amount is not an annotation of the first.
        let found: Vec<_> = amounts("Charged: SGD $25.00 (USD 18.50)")
            .into_iter()
            .map(|(money, _)| (money.number, money.currency.unwrap()))
            .collect();
        assert_eq!(
            found,
            [
                ("25.00".into(), "SGD".into()),
                ("18.50".into(), "USD".into())
            ]
        );
        // Arabic-Indic digits are not dates and must not panic.
        assert!(dates("٢٣/١١/٢٠٢٦ ٢٠٢٦-١١-٢٣ ٢٣ Nov", date("2026-11-24")).is_empty());
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
        let cut = truncate_lines(&text, 60);
        assert_eq!(cut, "x".repeat(40));
        assert!(amounts(cut).is_empty());
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
    fn reads_cited_card_and_account_numbers() {
        assert_eq!(
            cited_numbers(
                "Account number: 123456789. Card: 1234. A/C xxxx5678. Card ending 9999. Balance SGD 5."
            ),
            ["123456789", "1234", "5678", "9999"]
        );
        assert_eq!(
            cited_numbers(
                "Account number: 123-456789. Card: 4111 1111 1111 1234. Card: 1234 23 Nov. A/C 123-45678-9"
            ),
            ["123456789", "4111111111111234", "1234", "123456789"]
        );
        // The first group alone must not match an account ending in it.
        assert!(!names_number("Assets:Checking-123", "123456789"));
        assert!(names_number("Assets:Checking-6789", "123456789"));
        assert!(names_number("Liabilities:Citi-1234", "1234"));
        assert!(!names_number("Liabilities:Visa-91234567", "1234"));
        assert!(!names_number("Assets:Checking", "1234"));
        // A label and its number in separate HTML blocks.
        let html = "Message-ID: <c@bank.example>\r\nContent-Type: text/html\r\n\r\n\
<div>Card number:</div><div>5678</div><p>You spent SGD 3.00</p>\r\n";
        let email = parse(html.as_bytes()).unwrap();
        assert_eq!(cited_numbers(&email.body), ["5678"], "{:?}", email.body);
        // A loose "card" or "account" does not reach the next line, and an
        // amount's integer part is never a card number.
        assert!(cited_numbers("Credit to your card\n\n100.00 USD").is_empty());
        assert_eq!(
            cited_numbers("Credit card (5678) was charged USD 25.00"),
            ["5678"]
        );
        assert!(cited_numbers("Card: 100.00 USD").is_empty());
        assert_eq!(cited_numbers("Card: 5678 1000.00 USD"), ["5678"]);
        assert_eq!(cited_numbers("Card:\n5678"), ["5678"]);
        // Grouped account names compare as one number.
        let grouped = "Assets:Checking-123-45678-9012";
        assert!(!names_number(grouped, "987654329012"));
        assert!(names_number(grouped, "123456789012"));
        assert!(names_number(grouped, "9012"));
        assert!(names_number("Liabilities:Citi-1234", "4111111111111234"));
    }

    #[test]
    fn finds_unicode_payees() {
        assert_eq!(payees("Merchant: Café de Flore\n"), ["Café de Flore"]);
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

        // A placeholder plain-text alternative falls back to the HTML part.
        let alternative = "Message-ID: <a1@bank.example>\r\nSubject: Alert\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\n\
Please view this email in an HTML-capable client.\r\n--b\r\nContent-Type: text/html\r\n\r\n\
<p>You spent <b>SGD 12.50</b> at KOPITIAM.</p>\r\n--b--\r\n";
        let email = parse(alternative.as_bytes()).unwrap();
        assert!(email.body.contains("SGD 12.50"), "{:?}", email.body);

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
        // Every account shares the issuer name, which the email also mentions.
        let shared: BTreeMap<String, String> = (0..60)
            .map(|i| (format!("Assets:Bank-{i:02}"), String::new()))
            .chain([("Liabilities:Bank-Visa-91234".into(), String::new())])
            .collect();
        let kept = limit(shared, "Bank purchase on card ending 1234", |_| 1);
        assert!(kept.contains_key("Liabilities:Bank-Visa-91234"));

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
            // A transfer citing a missing debited account and a known credited one.
            (
                vec![
                    account("Assets:Savings-1234", &[], &[]),
                    account("Assets:Wallet", &[], &[]),
                ],
                vec![],
                "Transfer of $8.00 from account ending 5678 to account ending 1234 on 2026-11-23.",
            ),
            // No account names carry numbers at all.
            (
                vec![
                    account("Assets:Checking", &[], &[]),
                    account("Liabilities:Amex", &[], &[]),
                ],
                vec![],
                "Citi card ending 5678: you paid $8.00 to Grab on 2026-11-23.",
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
