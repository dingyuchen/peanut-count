// Monthly spending by expense category, computed with exact integer cents.
// An outflow to Expenses:X adds to X; a refund (inflow from Expenses:X) reduces it.

const TOP_CATEGORIES = 5;
const TREND_MONTHS = 6;

const cents = (amount) => BigInt(amount.replace('.', ''));

function shiftMonth(month, delta) {
  const [year, number] = month.split('-').map(Number);
  const index = year * 12 + (number - 1) + delta;
  return `${Math.floor(index / 12)}-${String((index % 12) + 1).padStart(2, '0')}`;
}

// statements: [{ currency, rows: [{ date, amount, account }] }]
// Returns { currencies, byCurrency: { USD: { months: { '2026-11': { total, categories: { Dining: 475n } } } } } }
function spendingIndex(statements) {
  const byCurrency = {};
  for (const statement of statements) {
    for (const row of statement.rows) {
      // Every month with a transaction exists, so months with only income still show as zero spending.
      const ledger = byCurrency[statement.currency] ??= { months: {} };
      const bucket = ledger.months[row.date.slice(0, 7)] ??= { total: 0n, categories: {} };
      if (!row.account.startsWith('Expenses:')) continue;
      const category = row.account.slice('Expenses:'.length);
      const spent = -cents(row.amount);
      bucket.total += spent;
      bucket.categories[category] = (bucket.categories[category] ?? 0n) + spent;
    }
  }
  const currencies = Object.keys(byCurrency).sort((a, b) =>
    Object.keys(byCurrency[b].months).length - Object.keys(byCurrency[a].months).length || a.localeCompare(b));
  return { currencies, byCurrency };
}

const latestMonth = (ledger) => Object.keys(ledger.months).sort().at(-1);
const earliestMonth = (ledger) => Object.keys(ledger.months).sort()[0];

// Whole percentages that add up to 100: floor each share, then hand the remaining
// points to the largest remainders (ties go to the larger value, which comes first).
function shares(values) {
  const total = values.reduce((sum, value) => sum + value, 0n);
  if (total <= 0n) return values.map(() => 0);
  const parts = values.map((value, index) => ({ index, floor: (value * 100n) / total, rest: (value * 100n) % total }));
  let left = 100 - parts.reduce((sum, part) => sum + Number(part.floor), 0);
  const result = parts.map((part) => Number(part.floor));
  for (const part of [...parts].sort((a, b) => (a.rest === b.rest ? a.index - b.index : a.rest > b.rest ? -1 : 1))) {
    if (left-- <= 0) break;
    result[part.index] += 1;
  }
  return result;
}

// The six months ending at `month`, oldest first, with months without data as zero.
function trend(ledger, month) {
  return Array.from({ length: TREND_MONTHS }, (_, index) => {
    const key = shiftMonth(month, index - TREND_MONTHS + 1);
    return { month: key, total: ledger.months[key]?.total ?? 0n };
  });
}

// Categories with positive net spending, largest first; the tail folds into "Other".
function breakdown(ledger, month) {
  const entries = Object.entries(ledger.months[month]?.categories ?? {})
    .filter(([, value]) => value > 0n)
    .sort(([nameA, a], [nameB, b]) => (a === b ? nameA.localeCompare(nameB) : a > b ? -1 : 1));
  if (entries.length <= TOP_CATEGORIES + 1) return entries.map(([name, value]) => ({ name, value }));
  const other = entries.slice(TOP_CATEGORIES).reduce((sum, [, value]) => sum + value, 0n);
  return [
    ...entries.slice(0, TOP_CATEGORIES).map(([name, value]) => ({ name, value })),
    { name: 'Other', value: other, other: true },
  ];
}

module.exports = { spendingIndex, latestMonth, earliestMonth, trend, breakdown, shares, shiftMonth };
