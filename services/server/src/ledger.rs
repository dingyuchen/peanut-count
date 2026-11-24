use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use bigdecimal::BigDecimal as Decimal;
use chrono::{Datelike, NaiveDate};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum Booking {
    #[default]
    Strict,
    Fifo,
}

impl Booking {
    fn name(self) -> &'static str {
        match self {
            Self::Strict => "STRICT",
            Self::Fifo => "FIFO",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Amount {
    pub number: String,
    pub currency: String,
}

impl Amount {
    fn parse(&self) -> Result<Decimal> {
        commodity(&self.currency)?;
        decimal(&self.number)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Posting {
    pub account: String,
    pub units: Amount,
    /// Per-unit acquisition cost, creating a new lot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Amount>,
    /// Per-unit sale or exchange price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Amount>,
    /// Book a sale against the account's existing lots.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sell: bool,
    /// Select a specific lot for a sale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lot: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Entry {
    Open {
        #[schemars(with = "String")]
        date: NaiveDate,
        account: String,
        #[serde(default)]
        currencies: Vec<String>,
        #[serde(default)]
        booking: Booking,
    },
    Close {
        #[schemars(with = "String")]
        date: NaiveDate,
        account: String,
    },
    Commodity {
        #[schemars(with = "String")]
        date: NaiveDate,
        currency: String,
    },
    Transaction {
        #[schemars(with = "String")]
        date: NaiveDate,
        #[serde(default)]
        payee: Option<String>,
        narration: String,
        postings: Vec<Posting>,
    },
    Balance {
        #[schemars(with = "String")]
        date: NaiveDate,
        account: String,
        amount: Amount,
    },
    Price {
        #[schemars(with = "String")]
        date: NaiveDate,
        currency: String,
        amount: Amount,
    },
    Reverse {
        #[schemars(with = "String")]
        date: NaiveDate,
        target: String,
    },
}

impl Entry {
    fn date(&self) -> NaiveDate {
        match *self {
            Self::Open { date, .. }
            | Self::Close { date, .. }
            | Self::Commodity { date, .. }
            | Self::Transaction { date, .. }
            | Self::Balance { date, .. }
            | Self::Price { date, .. }
            | Self::Reverse { date, .. } => date,
        }
    }

    fn order(&self) -> u8 {
        match self {
            Self::Open { .. } => 0,
            Self::Commodity { .. } => 1,
            Self::Balance { .. } => 2,
            Self::Transaction { .. } | Self::Reverse { .. } => 3,
            Self::Price { .. } => 4,
            Self::Close { .. } => 5,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Record {
    pub id: String,
    pub entry: Entry,
}

struct Account {
    currencies: Vec<String>,
    booking: Booking,
    closed: bool,
}

#[derive(Clone)]
struct Lot {
    account: String,
    currency: String,
    cost: Amount,
    date: NaiveDate,
    remaining: Decimal,
}

#[derive(Clone)]
struct BookedPosting {
    posting: Posting,
    lot: Option<String>,
    gain: Option<(String, Decimal)>,
}

#[derive(Clone)]
struct BookedTransaction {
    payee: Option<String>,
    narration: String,
    postings: Vec<BookedPosting>,
}

#[derive(Default)]
pub struct Ledger {
    accounts: HashMap<String, Account>,
    commodities: HashSet<String>,
    lots: HashMap<String, Lot>,
    lot_order: Vec<String>,
    balances: BTreeMap<(String, String), Decimal>,
    weights: BTreeMap<(String, String), Decimal>,
    costed_units: BTreeMap<(String, String), Decimal>,
    transactions: HashMap<String, BookedTransaction>,
    reversed: HashSet<String>,
    reversals: HashSet<String>,
    records: Vec<Record>,
}

impl Ledger {
    pub fn build(mut records: Vec<Record>) -> Result<Self> {
        records.sort_by_key(|record| (record.entry.date(), record.entry.order()));
        let mut ledger = Self::default();
        for record in &mut records {
            ledger
                .apply(record)
                .with_context(|| format!("entry {} ({})", record.id, record.entry.date()))?;
        }
        ledger.records = records;
        Ok(ledger)
    }

    /// Open accounts sorted by name, each with its declared currencies or,
    /// for an unconstrained account, the currencies it has been posted in.
    pub fn open_accounts(&self) -> Vec<(String, Vec<String>)> {
        let mut open: Vec<_> = self
            .accounts
            .iter()
            .filter(|(_, account)| !account.closed)
            .map(|(name, account)| {
                let currencies = if account.currencies.is_empty() {
                    self.balances
                        .keys()
                        .filter(|(held, _)| held == name)
                        .map(|(_, currency)| currency.clone())
                        .collect()
                } else {
                    account.currencies.clone()
                };
                (name.clone(), currencies)
            })
            .collect();
        open.sort();
        open
    }

    pub fn canonical_entry(&self, id: &str) -> Result<&Entry> {
        self.records
            .iter()
            .find(|record| record.id == id)
            .map(|record| &record.entry)
            .context("entry absent after validation")
    }

    fn apply(&mut self, record: &mut Record) -> Result<()> {
        let date = record.entry.date();
        ensure!(
            (1..=9999).contains(&date.year()),
            "date must have a four-digit positive year"
        );
        match &mut record.entry {
            Entry::Open {
                account,
                currencies,
                booking,
                ..
            } => {
                account_name(account)?;
                ensure!(
                    !self.accounts.contains_key(account),
                    "account {account} already exists"
                );
                let mut seen = HashSet::new();
                for currency in currencies.iter() {
                    commodity(currency)?;
                    ensure!(
                        seen.insert(currency),
                        "duplicate account currency {currency}"
                    );
                }
                self.accounts.insert(
                    account.clone(),
                    Account {
                        currencies: currencies.clone(),
                        booking: *booking,
                        closed: false,
                    },
                );
            }
            Entry::Close { account, .. } => {
                self.account(account, None)?;
                self.accounts.get_mut(account).unwrap().closed = true;
            }
            Entry::Commodity { currency, .. } => {
                commodity(currency)?;
                ensure!(
                    self.commodities.insert(currency.clone()),
                    "{currency} already declared"
                );
            }
            Entry::Transaction {
                payee,
                narration,
                postings,
                ..
            } => {
                text(narration)?;
                if let Some(payee) = payee {
                    text(payee)?;
                }
                ensure!(
                    postings.len() >= 2,
                    "a transaction needs at least two postings"
                );
                let mut booked = Vec::new();
                for posting in postings.iter() {
                    self.book(&record.id, date, posting, &mut booked)?;
                }
                self.post(&booked)?;
                *postings = booked.iter().map(|line| line.posting.clone()).collect();
                self.transactions.insert(
                    record.id.clone(),
                    BookedTransaction {
                        payee: payee.clone(),
                        narration: narration.clone(),
                        postings: booked,
                    },
                );
            }
            Entry::Balance {
                account, amount, ..
            } => {
                self.account(account, None)?;
                let expected = amount.parse()?;
                let actual: Decimal = self
                    .balances
                    .iter()
                    .filter(|((name, currency), _)| {
                        currency == &amount.currency && in_account(name, account)
                    })
                    .map(|(_, value)| value)
                    .sum();
                ensure!(
                    actual == expected,
                    "balance for {account} is {} {}, expected {}",
                    number(&actual),
                    amount.currency,
                    amount.number
                );
            }
            Entry::Price {
                currency, amount, ..
            } => {
                commodity(currency)?;
                ensure!(
                    currency != &amount.currency,
                    "price must name two different currencies"
                );
                positive(amount)?;
            }
            Entry::Reverse { target, .. } => {
                ensure!(
                    !self.reversed.contains(target),
                    "transaction {target} was already reversed"
                );
                let original = self
                    .transactions
                    .get(target)
                    .cloned()
                    .with_context(|| format!("no earlier transaction {target} to reverse"))?;
                ensure!(
                    !self.reversals.contains(target),
                    "a reversal cannot itself be reversed"
                );
                let mut postings = original.postings;
                for line in &mut postings {
                    let value = -line.posting.units.parse()?;
                    line.posting.units.number = number(&value);
                    self.account(&line.posting.account, Some(&line.posting.units.currency))?;
                    if let Some(id) = &line.lot {
                        self.adjust_lot(id, &value)?;
                    }
                    if let Some((_, gain)) = &mut line.gain {
                        *gain = -gain.clone();
                    }
                }
                self.post(&postings)?;
                self.transactions.insert(
                    record.id.clone(),
                    BookedTransaction {
                        payee: original.payee,
                        narration: format!("Reversal of {target}"),
                        postings,
                    },
                );
                self.reversed.insert(target.clone());
                self.reversals.insert(record.id.clone());
            }
        }
        Ok(())
    }

    fn account(&self, account: &str, currency: Option<&str>) -> Result<&Account> {
        let found = self
            .accounts
            .get(account)
            .with_context(|| format!("account {account} is not open"))?;
        ensure!(!found.closed, "account {account} is closed");
        if let Some(currency) = currency {
            ensure!(
                found.currencies.is_empty() || found.currencies.iter().any(|c| c == currency),
                "{account} does not allow {currency}"
            );
        }
        Ok(found)
    }

    fn book(
        &mut self,
        id: &str,
        date: NaiveDate,
        posting: &Posting,
        out: &mut Vec<BookedPosting>,
    ) -> Result<()> {
        let quantity = posting.units.parse()?;
        let booking = self
            .account(&posting.account, Some(&posting.units.currency))?
            .booking;
        let price = posting.price.as_ref().map(positive).transpose()?;
        let sell = posting.sell || posting.lot.is_some();
        if !sell {
            let lot = if let Some(cost) = &posting.cost {
                positive(cost)?;
                ensure!(
                    posting
                        .price
                        .as_ref()
                        .is_none_or(|price| price.currency == cost.currency),
                    "posting cost and price must use the same currency"
                );
                ensure!(
                    quantity > 0 && cost.currency != posting.units.currency,
                    "an acquisition needs positive units and a cost in a different currency"
                );
                let lot = format!("lot-{id}-{}", out.len() + 1);
                self.lots.insert(
                    lot.clone(),
                    Lot {
                        account: posting.account.clone(),
                        currency: posting.units.currency.clone(),
                        cost: cost.clone(),
                        date,
                        remaining: quantity,
                    },
                );
                self.lot_order.push(lot.clone());
                Some(lot)
            } else {
                None
            };
            out.push(BookedPosting {
                posting: posting.clone(),
                lot,
                gain: None,
            });
            return Ok(());
        }

        ensure!(
            quantity < 0 && posting.cost.is_none(),
            "a sale needs negative units and an existing lot"
        );
        let sale_price = posting
            .price
            .as_ref()
            .context("a sale needs a per-unit price")?;
        let price = price.unwrap();
        let mut candidates = Vec::new();
        if let Some(lot) = &posting.lot {
            let found = self
                .lots
                .get(lot)
                .with_context(|| format!("unknown lot {lot}"))?;
            ensure!(
                found.account == posting.account && found.currency == posting.units.currency,
                "{lot} does not hold {} in {}",
                posting.units.currency,
                posting.account
            );
            ensure!(
                found.cost.currency == sale_price.currency,
                "{lot} cost currency differs from sale price"
            );
            candidates.push(lot.clone());
        } else {
            candidates.extend(
                self.lot_order
                    .iter()
                    .filter(|id| {
                        let lot = &self.lots[*id];
                        lot.account == posting.account
                            && lot.currency == posting.units.currency
                            && lot.cost.currency == sale_price.currency
                            && lot.remaining > 0
                    })
                    .cloned(),
            );
        }
        let mut remaining = -quantity;
        let available: Decimal = candidates.iter().map(|id| &self.lots[id].remaining).sum();
        ensure!(
            available >= remaining,
            "insufficient lots of {} in {}",
            posting.units.currency,
            posting.account
        );
        // Strict booking permits an unambiguous single lot or a full liquidation.
        ensure!(
            matches!(booking, Booking::Fifo) || candidates.len() == 1 || available == remaining,
            "ambiguous sale in {}; specify a lot or use a FIFO account",
            posting.account
        );
        for lot in candidates {
            let matched = remaining.clone().min(self.lots[&lot].remaining.clone());
            let cost = self.lots[&lot].cost.parse()?;
            self.adjust_lot(&lot, &-matched.clone())?;
            let mut line = posting.clone();
            line.units.number = number(&-matched.clone());
            line.sell = true;
            line.lot = Some(lot.clone());
            let gain = (&price - cost) * &matched;
            out.push(BookedPosting {
                posting: line,
                lot: Some(lot),
                gain: Some((sale_price.currency.clone(), gain)),
            });
            remaining -= matched;
            if remaining == 0 {
                break;
            }
        }
        Ok(())
    }

    fn adjust_lot(&mut self, id: &str, delta: &Decimal) -> Result<()> {
        let lot = self
            .lots
            .get_mut(id)
            .with_context(|| format!("unknown lot {id}"))?;
        let balance = &lot.remaining + delta;
        ensure!(balance >= 0, "insufficient units in {id}");
        lot.remaining = balance;
        Ok(())
    }

    fn post(&mut self, lines: &[BookedPosting]) -> Result<()> {
        let mut weights = BTreeMap::<String, Decimal>::new();
        for line in lines {
            let posting = &line.posting;
            let units = posting.units.parse()?;
            let position = (posting.account.clone(), posting.units.currency.clone());
            let balance = self.balances.entry(position.clone()).or_default();
            *balance += &units;
            let costed = self.costed_units.entry(position).or_default();
            if line.lot.is_some() {
                *costed += &units;
            }
            ensure!(
                *costed == 0 || *balance == *costed,
                "cannot mix plain units and costed lots of {} in {}",
                posting.units.currency,
                posting.account
            );
            let (currency, value) = self.weight(line, units)?;
            let currency = currency.to_owned();
            *self
                .weights
                .entry((posting.account.clone(), currency.clone()))
                .or_default() += &value;
            *weights.entry(currency).or_default() += value;
        }
        for (currency, amount) in weights {
            ensure!(
                amount == 0,
                "transaction is out of balance by {} {currency}",
                number(&amount)
            );
        }
        Ok(())
    }

    fn weight<'a>(&'a self, line: &'a BookedPosting, units: Decimal) -> Result<(&'a str, Decimal)> {
        let posting = &line.posting;
        let weight = line
            .lot
            .as_ref()
            .map(|id| &self.lots[id].cost)
            .or(posting.price.as_ref());
        Ok(match weight {
            Some(amount) => (&amount.currency, amount.parse()? * units),
            None => (&posting.units.currency, units),
        })
    }

    pub fn validate_export(&self) -> Result<()> {
        // Beancount uses a 28-digit decimal context. Bounding the sum of absolute
        // values at their finest scale makes intermediate sums exact in any order.
        let mut range = BTreeMap::<String, (Decimal, i64)>::new();
        let mut add = |currency: &str, value: Decimal| -> Result<()> {
            let value = value.normalized();
            if value == 0 {
                return Ok(());
            }
            let scale = value.fractional_digit_count();
            let (total, max_scale) = range
                .entry(currency.to_owned())
                .or_insert_with(|| (Decimal::default(), scale));
            *max_scale = (*max_scale).max(scale);
            *total += value.abs();
            ensure!(
                total.with_scale(*max_scale).digits() <= 28,
                "Beancount's decimal precision is insufficient to export {currency} exactly"
            );
            Ok(())
        };
        for record in &self.records {
            match &record.entry {
                Entry::Balance { amount, .. } | Entry::Price { amount, .. } => {
                    add(&amount.currency, amount.parse()?)?;
                }
                Entry::Transaction { .. } | Entry::Reverse { .. } => {
                    for line in &self.transactions[&record.id].postings {
                        let units = line.posting.units.parse()?;
                        add(&line.posting.units.currency, units.clone())?;
                        let cost = line.lot.as_ref().map(|id| &self.lots[id].cost);
                        for amount in cost.into_iter().chain(line.posting.price.as_ref()) {
                            ensure!(
                                amount.parse()?.normalized().digits() <= 28,
                                "Beancount's decimal precision is insufficient to export {} exactly",
                                amount.currency
                            );
                        }
                        if cost.is_some() || line.posting.price.is_some() {
                            let (currency, value) = self.weight(line, units)?;
                            add(currency, value)?;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn report(&self, report: &str, account: Option<&str>) -> Result<Value> {
        let matches = |name: &str| account.is_none_or(|prefix| in_account(name, prefix));
        Ok(match report {
            "accounts" => json!(self.records.iter().filter(|record| {
                match &record.entry {
                    Entry::Open { account, .. } | Entry::Close { account, .. } => matches(account),
                    _ => false,
                }
            }).collect::<Vec<_>>()),
            "balances" | "trial-balance" => {
                let totals = if report == "balances" { &self.balances } else { &self.weights };
                json!(totals.iter()
                    .filter(|&((name, _), total)| matches(name) && *total != 0)
                    .map(|((name, currency), total)| json!({
                        "account": name, "currency": currency, "number": number(total)
                    }))
                    .collect::<Vec<_>>())
            }
            "transactions" => json!(self.records.iter().filter_map(|record| {
                let transaction = self.transactions.get(&record.id)?;
                transaction.postings.iter().any(|line| matches(&line.posting.account)).then(|| {
                    json!({
                        "id": record.id, "date": record.entry.date(), "payee": transaction.payee,
                        "narration": transaction.narration,
                        "reversal_of": match &record.entry { Entry::Reverse { target, .. } => Some(target), _ => None },
                        "postings": transaction.postings.iter().map(|line| {
                            json!({
                                "account": line.posting.account, "units": line.posting.units,
                                "cost": line.lot.as_ref().map(|id| &self.lots[id].cost),
                                "price": line.posting.price, "lot": line.lot
                            })
                        }).collect::<Vec<_>>()
                    })
                })
            }).collect::<Vec<_>>()),
            "holdings" => json!(self.lot_order.iter().filter_map(|id| {
                let lot = &self.lots[id];
                (matches(&lot.account) && lot.remaining != 0).then(|| json!({
                    "lot": id, "account": lot.account, "currency": lot.currency,
                    "units": number(&lot.remaining), "cost": lot.cost, "date": lot.date
                }))
            }).collect::<Vec<_>>()),
            "gains" => {
                let mut gains = Vec::new();
                for record in &self.records {
                    if let Some(transaction) = self.transactions.get(&record.id) {
                        for line in &transaction.postings {
                            if let Some((currency, value)) = &line.gain
                                && matches(&line.posting.account)
                            {
                                gains.push(json!({
                                    "id": record.id, "date": record.entry.date(),
                                    "account": line.posting.account, "lot": line.lot,
                                    "currency": currency, "number": number(value)
                                }));
                            }
                        }
                    }
                }
                json!(gains)
            },
            "prices" => json!(self.records.iter().filter(|record| matches!(record.entry, Entry::Price { .. })).collect::<Vec<_>>()),
            _ => bail!("unknown report {report}"),
        })
    }

    pub fn export(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for record in &self.records {
            let date = record.entry.date();
            match &record.entry {
                Entry::Open {
                    account,
                    currencies,
                    booking,
                    ..
                } => {
                    write!(out, "{date} open {account}").unwrap();
                    if !currencies.is_empty() {
                        write!(out, " {}", currencies.join(",")).unwrap();
                    }
                    writeln!(out, " \"{}\"", booking.name()).unwrap();
                }
                Entry::Close { account, .. } => writeln!(out, "{date} close {account}").unwrap(),
                Entry::Commodity { currency, .. } => {
                    writeln!(out, "{date} commodity {currency}").unwrap()
                }
                Entry::Balance {
                    account, amount, ..
                } => {
                    writeln!(
                        out,
                        "{date} balance {account} {} {}",
                        amount.number, amount.currency
                    )
                    .unwrap();
                }
                Entry::Price {
                    currency, amount, ..
                } => {
                    writeln!(
                        out,
                        "{date} price {currency} {} {}",
                        amount.number, amount.currency
                    )
                    .unwrap();
                }
                Entry::Transaction { .. } | Entry::Reverse { .. } => {
                    let transaction = &self.transactions[&record.id];
                    write!(out, "{date} * ").unwrap();
                    if let Some(payee) = &transaction.payee {
                        write!(out, "{} ", quoted(payee)).unwrap();
                    }
                    writeln!(out, "{}", quoted(&transaction.narration)).unwrap();
                    for line in &transaction.postings {
                        let posting = &line.posting;
                        write!(
                            out,
                            "  {}  {} {}",
                            posting.account, posting.units.number, posting.units.currency
                        )
                        .unwrap();
                        if let Some(id) = &line.lot {
                            let lot = &self.lots[id];
                            write!(
                                out,
                                " {{{} {}, {}, \"{}\"}}",
                                lot.cost.number, lot.cost.currency, lot.date, id
                            )
                            .unwrap();
                        }
                        if let Some(price) = &posting.price {
                            write!(out, " @ {} {}", price.number, price.currency).unwrap();
                        }
                        out.push('\n');
                    }
                    out.push('\n');
                }
            }
        }
        out
    }
}

fn in_account(name: &str, prefix: &str) -> bool {
    name == prefix
        || name
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with(':'))
}

fn account_name(name: &str) -> Result<()> {
    let (root, rest) = name
        .split_once(':')
        .context("account must have a root and subaccount")?;
    ensure!(
        matches!(
            root,
            "Assets" | "Liabilities" | "Equity" | "Income" | "Expenses"
        ) && name.len() <= 255
            && rest.split(':').all(|part| {
                part.as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                    && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }),
        "invalid Beancount account {name}"
    );
    Ok(())
}

fn commodity(value: &str) -> Result<()> {
    let valid = |b: u8| b.is_ascii_uppercase() || b.is_ascii_digit();
    ensure!(
        (1..=24).contains(&value.len())
            && value.as_bytes()[0].is_ascii_uppercase()
            && valid(value.as_bytes()[value.len() - 1])
            && value
                .bytes()
                .all(|b| valid(b) || matches!(b, b'_' | b'-' | b'.' | b'\'')),
        "invalid commodity {value}"
    );
    Ok(())
}

fn positive(amount: &Amount) -> Result<Decimal> {
    let value = amount.parse()?;
    ensure!(value > 0, "cost and price must be positive");
    Ok(value)
}

fn decimal(input: &str) -> Result<Decimal> {
    let unsigned = input.strip_prefix('-').unwrap_or(input);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    ensure!(
        !whole.is_empty()
            && whole.len() <= 38
            && fraction.len() <= 18
            && (!unsigned.contains('.') || !fraction.is_empty())
            && whole
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit()),
        "invalid decimal amount {input:?}"
    );
    input.parse().context("invalid decimal")
}

fn number(amount: &Decimal) -> String {
    amount.normalized().to_plain_string()
}

fn text(value: &str) -> Result<()> {
    ensure!(
        value.len() <= 4096 && !value.chars().any(char::is_control),
        "text must be at most 4096 bytes and contain no control characters"
    );
    Ok(())
}

fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
